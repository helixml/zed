//! Process-global cache of `AgentConnection`s keyed by `(Project, AgentId)`.
//!
//! Without this cache, multiple call sites can race on `AgentServer::connect()`
//! for the same agent in the same project — each `connect()` spawns a fresh
//! `claude-agent-acp` (or equivalent) wrapper, which spawns its own `claude
//! --resume <session>` and its own MCP children. Two such races have been
//! observed:
//!
//! - Inside `external_websocket_sync::thread_service`, where `create_new_thread_sync`,
//!   `load_thread_from_agent`, and `open_existing_thread_sync` each call
//!   `server.connect()` independently and can fire concurrently when workspace
//!   restore overlaps with an inbound `open_thread` WebSocket command.
//! - Between `agent_ui::AgentConnectionStore` and the above, when both UI and
//!   external paths target the same agent.
//!
//! Symptom: duplicate `claude --resume <id>` processes scribbling on the same
//! on-disk session file, two MCP children competing on the same npx cache, and
//! the model talking to whichever claude got the broken half (most visibly:
//! missing `chrome-devtools-mcp` tools).
//!
//! Concurrent callers for the same `(project, agent_id)` share a single
//! in-flight connect Task via `futures::future::Shared`. Once resolved, the
//! cached entry serves subsequent callers until eviction. Failed connects are
//! evicted so retry can re-attempt.

use std::{cell::RefCell, rc::Rc};

use acp_thread::{AgentConnection, LoadError};
use collections::HashMap;
use futures::{FutureExt, future::Shared};
use gpui::{App, Entity, EntityId, Global, SharedString, Task};
use project::{AgentId, Project};

use crate::{AgentServer, AgentServerDelegate};

/// Stored result type. `LoadError` is `Clone`, which `Shared` requires.
pub type CachedConnectResult = Result<Rc<dyn AgentConnection>, LoadError>;

/// Process-global cache. One entry per `(Project entity_id, AgentId)`.
pub struct AgentConnectionCache {
    entries: RefCell<HashMap<CacheKey, Shared<Task<CachedConnectResult>>>>,
}

impl Global for AgentConnectionCache {}

#[derive(Clone, Hash, Eq, PartialEq)]
struct CacheKey {
    project: EntityId,
    agent_id: AgentId,
}

/// Install the cache as a GPUI global. Idempotent.
pub fn init(cx: &mut App) {
    if cx.try_global::<AgentConnectionCache>().is_none() {
        cx.set_global(AgentConnectionCache {
            entries: RefCell::new(HashMap::default()),
        });
    }
}

impl AgentConnectionCache {
    /// Get the cached `AgentConnection` for `(project, agent_id)`, or call
    /// `server.connect()` and cache the result.
    ///
    /// Returns a `Shared<Task>` so concurrent callers awaiting it all observe
    /// the same outcome — first caller drives the connect, others wait. Once
    /// the underlying task resolves, the `Shared` caches the value and later
    /// callers get it without re-spawning anything.
    ///
    /// On error, the entry is evicted from the cache so retry can occur.
    ///
    /// Note on `delegate`: the first caller's `AgentServerDelegate` (and its
    /// `new_version_available` watch sender) is the one actually wired up.
    /// Subsequent callers' delegates are dropped unused. This is best-effort
    /// for the version-notification path; the core load behaviour is unaffected.
    pub fn request_connection(
        cx: &mut App,
        project: Entity<Project>,
        agent_id: AgentId,
        server: Rc<dyn AgentServer>,
        delegate: AgentServerDelegate,
    ) -> Shared<Task<CachedConnectResult>> {
        init(cx);

        let key = CacheKey {
            project: project.entity_id(),
            agent_id: agent_id.clone(),
        };

        // Fast path: an entry exists (in-flight or resolved). `Shared` handles
        // both — awaiting a resolved Shared returns the cached value instantly.
        {
            let cache = cx.global::<AgentConnectionCache>();
            if let Some(existing) = cache.entries.borrow().get(&key) {
                log::info!(
                    "[ACP_DEDUP] Reusing connection project={} agent={:?}",
                    key.project, key.agent_id
                );
                return existing.clone();
            }
        }

        log::info!(
            "[ACP_DEDUP] No cached connection, calling server.connect() project={} agent={:?}",
            key.project, key.agent_id
        );

        // Slow path: spawn a fresh connect, store the Shared task, hand it back.
        let project_for_connect = project.clone();
        let connect_task = server.connect(delegate, project_for_connect, cx);
        let connect_task: Task<CachedConnectResult> = cx.spawn(async move |_| {
            connect_task
                .await
                .map_err(|e| LoadError::Other(SharedString::from(e.to_string())))
        });
        let shared = connect_task.shared();

        cx.global_mut::<AgentConnectionCache>()
            .entries
            .borrow_mut()
            .insert(key.clone(), shared.clone());

        // Evict on failure so a retry can create a fresh connection. On success
        // we leave the entry in place — the resolved Shared serves subsequent
        // callers from cache.
        let shared_for_completion = shared.clone();
        cx.spawn(async move |cx| {
            if let Err(err) = shared_for_completion.await {
                log::warn!(
                    "[ACP_DEDUP] Connection failed, evicting entry project={} agent={:?}: {:?}",
                    key.project, key.agent_id, err
                );
                cx.update(|cx| {
                    if let Some(cache) = cx.try_global::<AgentConnectionCache>() {
                        cache.entries.borrow_mut().remove(&key);
                    }
                });
            }
        })
        .detach();

        shared
    }

    /// Drop all cached entries for a project. Useful when the project is
    /// closing and any in-flight connections should be abandoned.
    pub fn drop_project(cx: &mut App, project_entity_id: EntityId) {
        if let Some(cache) = cx.try_global::<AgentConnectionCache>() {
            cache
                .entries
                .borrow_mut()
                .retain(|key, _| key.project != project_entity_id);
        }
    }

    /// Synchronously evict a single cache entry. Use when a caller observed
    /// a failure and intends to retry immediately, so the next call doesn't
    /// see the cached failed `Shared`.
    pub fn evict(cx: &mut App, project_entity_id: EntityId, agent_id: AgentId) {
        if let Some(cache) = cx.try_global::<AgentConnectionCache>() {
            let key = CacheKey {
                project: project_entity_id,
                agent_id,
            };
            cache.entries.borrow_mut().remove(&key);
        }
    }
}
