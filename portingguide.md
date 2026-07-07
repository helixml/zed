# Helix Fork Porting Guide

This document describes all Helix-specific changes to the Zed codebase and the critical fixes needed when rebasing or updating the fork against upstream Zed. It serves as a checklist to ensure nothing is lost during future rebases.

## Overview

The Helix fork adds a WebSocket-based bidirectional sync layer between Zed and the Helix API server. This enables Helix to send chat messages to Zed's agent panel and receive streaming responses, thread lifecycle events, and UI state queries — all without modifying Zed's core agent/thread architecture.

**Design principle:** All Helix changes are behind `#[cfg(feature = "external_websocket_sync")]` feature gates where possible, minimizing merge conflicts with upstream.

## Architecture

```
Helix API Server
    ↕ WebSocket (bidirectional)
Zed (external_websocket_sync crate)
    ↕ GPUI entities + callbacks
Zed Agent Panel (agent_ui crate)
    ↕ AgentConnection trait
NativeAgent / ACP Agent (agent crate)
    ↕ LLM API
Claude / Qwen / etc.
```

## Helix-Specific Crates

### `crates/external_websocket_sync/`

The entire crate is Helix-specific. It provides:

| File | Purpose |
|------|---------|
| `external_websocket_sync.rs` | Crate root: global callback channels, init functions, public API |
| `websocket_sync.rs` | WebSocket client: connect, reconnect, send/receive messages |
| `thread_service.rs` | Thread lifecycle: create, follow-up, load, open threads via GPUI |
| `types.rs` | `SyncEvent` enum for all WebSocket event types |
| `sync_settings/` | Settings module: `ZED_HELIX_URL`, TLS config, etc. |
| `mock_helix_server.rs` | In-process mock server for unit tests |
| `protocol_test.rs` | Protocol-level integration tests |
| `server.rs` | WebSocket server utilities |
| `mcp.rs` | MCP integration helpers |
| `e2e-test/` | Docker-based E2E test with real LLM calls |

### E2E Test (`e2e-test/`)

Ten-phase test that validates the full protocol for both `zed-agent` and `claude` (Claude Code) agents. Runs in Docker against a real LLM (Anthropic API). The Go test server uses the **same production Helix handler code** (`NewTestServer` + `ExternalAgentSyncHandler`) with an in-memory store.

Two Dockerfiles:
- `Dockerfile.runtime` — for local dev runs (`run_docker_e2e.sh`)
- `Dockerfile.ci` — for CI (takes pre-built Zed binary + Helix Go source as build context)

**Important:** The test creates a seed session in the store before Zed connects (matching `HELIX_SESSION_ID`). This mirrors production where sessions always exist before the agent connects. Without it, `handleUserCreatedThread` fails with "session not found". See `CLAUDE.md` in the e2e-test directory for binary freshness requirements.

Phases:
1. **Phase 1**: New thread creation via `chat_message`
2. **Phase 2**: Follow-up message to existing thread
3. **Phase 3**: New thread creation (second thread)
4. **Phase 4**: Follow-up to non-visible thread (Thread A while Thread B is displayed)
5. **Phase 5**: Simulate user input (Zed → Helix sync direction)
6. **Phase 6**: Query UI state (active_view, thread_id, entry_count, MCP servers, model)
7. **Phase 7**: Open thread + follow-up chat
8. **Phase 8**: Mid-stream interrupt (second `send()` displaces active turn, both emit `Stopped`)
9. **Phase 9**: Rapid 3-turn cancel (chat_message, then simulate_user_input + chat_message back-to-back)
10. **Phase 10**: User-created thread (inject `user_created_thread`, verify session + work session)

A `slow-mcp-server` test helper (in `e2e-test/slow-mcp-server/`) simulates an MCP server with delayed tool responses, used to test the `wait_for_tools_ready` path (Phase 1 waits ~30s for MCP tools to load).

Claude Code (`claude-agent-acp`) is auto-installed from npm by Zed at runtime — the test does NOT bundle a local copy. The version is logged at test start for debugging.

```bash
# Run E2E test (local) — ALWAYS copy latest binary first!
cd crates/external_websocket_sync/e2e-test
cp ~/pm/helix/zed-build/zed zed-binary
./run_docker_e2e.sh                          # zed-agent only
E2E_AGENTS="zed-agent,claude" ./run_docker_e2e.sh  # both agents
# Screenshots saved to ./screenshots/
```

## Modified Upstream Files

These files contain Helix-specific changes that must be preserved during rebases:

### `Cargo.toml` (workspace root)
- Added `crates/external_websocket_sync` to workspace members
- Added `external_websocket_sync` workspace dependency

### `crates/zed/Cargo.toml`
- Added `external_websocket_sync` feature flag
- Added `external_websocket_sync` optional dependency

### `crates/zed/src/zed.rs`
- Initialization of WebSocket sync service on startup (cfg-gated)

### `crates/zed/src/main.rs`
- `--headless` CLI flag and `initialize_headless()` function. Allows Zed to run with no
  display server (no Wayland, no X11) and no windows, while still initializing the
  external WebSocket sync (Helix) and the agent/MCP backend. Specifically:
  - Adds `headless: bool` to the `Args` struct.
  - Passes `args.headless` to `gpui_platform::current_platform(...)` so GPUI's Linux
    layer uses `HeadlessClient` (no X11/Wayland connection, no window opening).
  - Treats `--headless` as implying `--allow-multiple-instances` in the
    `failed_single_instance_check` short-circuit (otherwise the singleton lock would
    block headless backends from running alongside a desktop Zed).
  - In the `app.run` callback, after global init but before workspace creation,
    branches on `args.headless` and calls `initialize_headless()`. That function
    constructs a windowless `Project::local`, grabs the global `ThreadStore`, calls
    `external_websocket_sync::setup_thread_handler`, and then starts the WebSocket
    service via `init_websocket_service` if `ExternalSyncSettings` says it's enabled.
    Then it returns; the GPUI event loop continues running the WebSocket / thread
    tasks until the process is signalled.
  - The body of `initialize_headless` is `cfg(feature = "external_websocket_sync")`-
    gated; without the feature it just logs a warning and idles.
- Also adds `--allow-multiple-instances` (older Helix-only flag — see Critical Fix
  #39 below).

### `crates/agent_ui/Cargo.toml`
- Added `external_websocket_sync` feature flag
- Added `external_websocket_sync_dep` optional dependency

### `crates/agent_ui/src/agent_panel.rs`
- **Thread display callback**: Receives `ThreadDisplayNotification` from thread_service, calls `from_existing_thread()` to display threads in the panel. Passes `this.connection_store.clone()` and `crate::Agent::NativeAgent` to the constructor (required since the 2026-03-22 upstream merge added these fields to `ConversationView`).
- **UI state query callback**: Responds to `query_ui_state` with current active_view, thread_id, entry_count, `mcp_servers` map, and `active_model` string. Matches `ActiveView::AgentThread { conversation_view }` (not `server_view` — field was renamed in upstream 2026-03-22 merge).
- **Thread creation callback**: Wires up thread_service to create threads
- **Thread open callback**: Wires up thread_service to open existing threads
- **Onboarding dismissal**: Auto-dismisses `OnboardingUpsell` when WebSocket sync is active
- **`acp_history_store()`**: Accessor for `ThreadStore` entity, used by WebSocket integration setup (cfg-gated)
- **Entity-level split-brain detection**: In `ThreadDisplayNotification` handler, compares `Entity` references (not just session IDs) to detect container-restart split-brain where the same thread ID has a new entity. Match on `conversation_view` (not `server_view`) in `ActiveView::AgentThread`.
- **Auto-follow activation**: After `set_active_view`, calls `workspace.follow(CollaboratorId::Agent)` if `should_be_following` is true — both for new threads and follow-up messages via the "same entity" path
- **History from connection_store**: `ThreadDisplayNotification` reads history via `this.connection_store.read(cx).entry(&Agent::NativeAgent).and_then(|e| e.read(cx).history().cloned())` — backed by `AcpSessionList`, not `NativeAgentSessionList`.

### `crates/agent_ui/src/conversation_view.rs`

> **Note:** This was previously `crates/agent_ui/src/acp/thread_view.rs`. The upstream 2026-03-22 merge renamed the `acp` module to `conversation_view`. All Helix changes moved with it.

- **`HeadlessConnection`**: No-op `AgentConnection` impl for WebSocket-created threads (cfg-gated). Must implement `agent_id()` and `new_session()` — their signatures must track the `AgentConnection` trait. Default impls handle `wait_for_tools_ready()`.
- **`from_existing_thread()` constructor**: Creates a `ConversationView` wrapping an existing `Entity<AcpThread>` with a `HeadlessConnection`. Uses `ConnectedServerState` with `connection`, `auth_state`, `active_id`, `threads` HashMap, `conversation` Entity, `history`, and `_connection_entry_subscription` (use `Subscription::new(|| {})`). Takes `connection_store` and `connection_key` parameters.
- **Thread registry integration**: Registers threads from both `from_existing_thread` and the connected state into `THREAD_REGISTRY`
- **History refresh**: Calls `self.history().update(cx, |h, cx| h.refresh(cx))` on `Stopped` events — note `history` is now a method (`history()`) not a field, and must guard with `if let Some(history) = self.history()`.
- **Thread unregistration on reset/drop**: Calls `external_websocket_sync::unregister_thread()` when the view resets or the entity changes
- **`is_resume` flag**: Uses `load_session_id.is_some()` (not the removed `resume_thread` variable) to determine whether a thread is being resumed vs created new, for the `UserCreatedThread` WebSocket event gate

### `crates/agent_ui/src/config_options.rs`

> **Note:** Previously `crates/agent_ui/src/acp/config_options.rs`.

- **`current_model_value()` method**: Returns the current model ID string from the `SessionConfigOptionCategory::Model` config option. Used by `thread_view.rs` `current_model_id()` fallback path

### `crates/agent_ui/src/conversation_view/thread_view.rs`

> **Note:** Previously `crates/agent_ui/src/acp/thread_view/active_thread.rs`.

- **`current_model_id()` fallback chain**: Now tries (1) model_selector, (2) config_options_view via `current_model_value()`, (3) global `LanguageModelRegistry::read_global()` default. This ensures headless/external threads report a model ID in UI state queries

### `crates/extensions_ui/src/extensions_ui.rs`
- **Agent keyword removal**: Claude/Codex/Gemini keywords removed from search (enterprise — users should use corporate LLMs)
- **Agent upsell removal**: Claude/Codex/Gemini upsell banners removed from extensions UI

### `crates/recent_projects/src/dev_container_suggest.rs`
- **`suggest_dev_container` check**: Early return if `RemoteSettings::suggest_dev_container` is false

### `crates/feature_flags/src/flags.rs`
- **ACP beta feature flag override**: `AcpBetaFeatureFlag::enabled_for_all()` returns `true` to enable session list/load/resume in release builds

### `crates/acp_thread/src/acp_thread.rs`
- **`content_only()` method on `AssistantMessage`**: Returns content without the `## Assistant\n\n` heading. Used by thread_service.rs for WebSocket sync to avoid sending the heading to Helix.
- **`AcpThreadEvent::Stopped` is a tuple variant**: As of the 2026-03-22 upstream merge, `Stopped` takes a `StopReason` argument: `Stopped(acp::StopReason)`. Pattern matches must use `Stopped(_)` and emission must pass a reason, e.g. `cx.emit(AcpThreadEvent::Stopped(acp::StopReason::Cancelled))`.
- **`cancel()` drops send_task instead of awaiting**: See Critical Fix #8 below.
- **`run_turn()` normal completion guards Stopped with `stopped_emitted`**: See Critical Fix #9 below.

### `crates/acp_thread/src/connection.rs`
- **`wait_for_tools_ready()` on `AgentConnection` trait**: New method added to `AgentConnection`. Default impl returns `Task::ready(())`. `HeadlessConnection` relies on the default. `NativeAgentConnection` implementation in `context_server_registry.rs` waits for all pending MCP tool loads. **When upstream adds methods to `AgentConnection`, `HeadlessConnection` must be updated** — it won't compile otherwise.
- **`new_session()` takes `PathList` not `&Path`**: As of 2026-03-22, signature is `new_session(self: Rc<Self>, project: Entity<Project>, work_dirs: PathList, cx: &mut App)`. Use `PathList::new(&[cwd.clone()])` to construct from a `PathBuf`.
- **`load_session()` signature changed**: Now `load_session(self: Rc<Self>, session_id: acp::SessionId, project: Entity<Project>, work_dirs: PathList, title: Option<SharedString>, cx: &mut App)`. The old `AgentSessionInfo` wrapper is gone — pass `acp::SessionId::new(id)` directly.

### `crates/agent_servers/`
- **`AgentServerDelegate::new` takes 2 args**: As of 2026-03-22, signature is `new(store: Entity<AgentServerStore>, new_version_tx: Option<watch::Sender<Option<String>>>)`. The `project` and `status_tx` parameters were removed.
- **`AgentServer::connect` takes 3 args and returns `Task<Result<Rc<dyn AgentConnection>>>`**: Signature is `connect(delegate, project: Entity<Project>, cx)`. No longer returns a tuple — just `Rc<dyn AgentConnection>`.
- **`Gemini` and `ClaudeCode` structs removed**: Use `CustomAgentServer::new(AgentId("gemini-cli".into()))` and `CustomAgentServer::new(AgentId("claude".into()))` respectively.
- **`CustomAgentServer::new` takes `AgentId`**: Not `SharedString`. Use `AgentId(name.clone())`.

### `crates/agent/src/agent.rs`
- **`load_session()` entity lifetime fix**: Clones `Entity<NativeAgent>` to keep it alive during async `open_thread` task (see Critical Fixes below)
- **Multi-project `NativeAgent`**: Upstream restructured `NativeAgent` to support multiple projects: `projects: HashMap<EntityId, ProjectState>` where each `ProjectState` has `context_server_registry` and `project` fields. The old flat `agent.project` and `agent.context_server_registry()` no longer exist. `wait_for_tools_ready` uses `agent.projects.values().next()` to get the first `ProjectState`.
- **`wait_for_tools_ready` accesses `ProjectState`**: Use `project_state.context_server_registry.read(cx)` and `project_state.project.read(cx).context_server_store()` when implementing tools-ready logic.

### `crates/agent/src/agent.rs`
- **`load_session()` entity lifetime fix**: Clones `Entity<NativeAgent>` to keep it alive during async `open_thread` task (see Critical Fixes below)

### `crates/agent/src/tools/grep_tool.rs`
- **Line truncation**: `truncate_long_lines()` helper caps grep output at 500 chars per line with `[truncated, N chars total]` indicator. Prevents context window blowups when grepping minified files.

### `crates/agent/src/tools/context_server_registry.rs`
- **MCP tools-ready tracking**: Added `pending_tool_loads: usize`, `pending_server_starts: HashSet<ContextServerId>`, and `tools_ready_tx: watch::Sender<usize>` to track when all MCP servers have finished loading tools. Implements `wait_for_tools_ready()` for `NativeAgentConnection` by watching for `pending_tool_loads` to reach zero.

### `crates/workspace/src/workspace.rs`
- **Agent follow doesn't steal keyboard focus**: In `follow()` and `update_follower_items()`, added `!matches!(leader_id, CollaboratorId::Agent)` guard before `window.focus(...)` calls. When following the agent, Zed tracks the agent's active file visually without stealing keyboard focus from the user's current input. **Critical: upstream will modify `follow()` frequently — this guard must be re-checked after every merge.**

### `crates/zed/src/zed/migrate.rs`
- **Migration banner hidden in Helix builds**: Early return `ToolbarItemLocation::Hidden` when `cfg!(feature = "external_websocket_sync")`. In Helix, settings are managed by the settings-sync-daemon and the migration prompt is irrelevant.

### `crates/title_bar/`
- **Helix connection status indicator**: Shows WebSocket connection status in the title bar
- **`external_websocket_sync` must be optional**: In `title_bar/Cargo.toml`, the dep must be `external_websocket_sync = { workspace = true, optional = true }` and the `[features]` section must include `external_websocket_sync = ["dep:external_websocket_sync"]`. Without this, `#[cfg(feature = "external_websocket_sync")]` always evaluates to false and the icon never renders.
- **Feature propagation**: `crates/zed/Cargo.toml`'s `external_websocket_sync` feature must include `"title_bar/external_websocket_sync"` to enable the feature when building with Helix support.

### `crates/http_client_tls/src/http_client_tls.rs`
- **`NoCertVerifier`**: Skips TLS certificate verification when `ZED_HTTP_INSECURE_TLS=1`
- For enterprise deployments with internal CAs / self-signed certs

### `crates/reqwest_client/src/reqwest_client.rs`
- **Insecure TLS support**: Reads `ZED_HTTP_INSECURE_TLS=1` to disable cert verification

### `crates/agent_settings/src/agent_settings.rs`
- **`show_onboarding`**: Setting to control onboarding visibility
- **`auto_open_panel`**: Setting to control agent panel auto-open

### ~~`crates/context_server/src/client.rs`~~ (no longer modified — see retired Critical Fix #10)

### `.dockerignore`
- Simplified for Helix build context

## Critical Fixes (Must Be Preserved)

These fixes address subtle bugs that are easy to lose during rebases because they're small changes to upstream code. Each has been verified with E2E tests.

### 1. Keep NativeAgent Entity Alive During `load_session`

**File:** `crates/agent/src/agent.rs` — `NativeAgentConnection::load_session()`

**Bug:** When `load_session()` is called (e.g., after Zed restart to reload a thread), the `Rc<NativeAgentConnection>` is consumed. Inside `open_thread()`, the async task captures `this` as a `WeakEntity<NativeAgent>`. Once the `Rc` is dropped, the `WeakEntity` can't upgrade → "entity released" error.

**Fix:** Clone `Entity<NativeAgent>` before spawning the async task, keep it alive until the task completes:

```rust
fn load_session(self: Rc<Self>, session: AgentSessionInfo, ..., cx: &mut App)
    -> Task<Result<Entity<acp_thread::AcpThread>>>
{
    let agent = self.0.clone();  // Keep strong reference
    let task = self.0.update(cx, |a, cx| a.open_thread(session.session_id, cx));
    cx.spawn(async move |_cx| {
        let result = task.await;
        drop(agent);  // Release after task completes
        result
    })
}
```

**History:** Originally fixed in old fork commit `bc721cd`, lost during rebase, re-applied as `0a78bf8`.

**Symptom:** "Thread load failed: Failed to load thread: entity released" after Zed restart.

### 2. No Duplicate WebSocket Event Sends

**File:** `crates/agent_ui/src/acp/thread_view.rs`

**Bug:** Both `thread_service.rs` AND `thread_view.rs` subscribe to thread events (`NewEntry`, `EntryUpdated`, `Stopped`) and send `MessageAdded`/`MessageCompleted` WebSocket events, causing duplicate messages in the Helix chat.

**Fix:** `thread_service.rs` is the canonical source for WebSocket events. `thread_view.rs` must NOT send `MessageAdded`, `MessageCompleted`, or streaming `EntryUpdated` events. It should only send UI-specific events:
- `UserCreatedThread` (user created thread via UI)
- `ThreadTitleChanged` (title updated)

**History:** Commit `cc037db` moved event sending to thread_service.rs, but thread_view.rs events were not removed during the port. Fixed in `72e2952`.

**Symptom:** Every assistant message appears twice in the Helix Sessions chat.

### 3. Strip "## Assistant" Heading from Synced Messages

**File:** `crates/acp_thread/src/acp_thread.rs`, `crates/external_websocket_sync/src/thread_service.rs`

**Bug:** `AssistantMessage::to_markdown()` wraps content with `## Assistant\n\n...\n\n`. When synced to Helix, every response starts with a "## Assistant" heading.

**Fix:** Added `content_only()` method that returns just the chunks without the heading. All `msg.to_markdown(cx)` calls in `thread_service.rs` (for `AssistantMessage`) use `msg.content_only(cx)` instead.

**History:** Old fork had this fix, lost during rebase. Re-applied as `98ec442`.

**Symptom:** Every assistant response in Helix starts with "## Assistant" heading.

### 4. Follow-up to Non-Visible Thread Must Notify UI

**File:** `crates/external_websocket_sync/src/thread_service.rs`

**Bug:** When a `chat_message` targets a thread that exists in `THREAD_REGISTRY` but is not currently displayed (e.g., Thread A while Thread B is visible), the message is sent but the UI doesn't switch to show the response.

**Fix:** Before sending a follow-up message, call `notify_thread_display()` to tell the agent panel to switch to the target thread.

**History:** Added in `fb96f34`. Tested by E2E Phase 4.

**Symptom:** Follow-up message sent to hidden thread, but UI stays on the wrong thread.

### 5. Flush Stale Pending Entries When a Different Entry Starts Streaming

**File:** `crates/external_websocket_sync/src/thread_service.rs`

**Bug:** When two entries stream concurrently (e.g., a tool call overlaps with a text entry), the throttle buffer can hold a stale pending message for the old entry while a new entry starts. The stale message is then sent out of order or dropped.

**Fix:** At the start of each streaming update, check whether the incoming `message_id` differs from the buffered pending message. If so, flush all stale pending entries for other `message_id`s before processing the new entry. This preserves ordering and ensures every entry's content reaches Helix.

**History:** Added in `6e4967240a`. Required by multi-tool-call E2E test scenarios.

**Symptom:** Tool call results appear out of order or are missing from the Helix session view.

### 6. AcpThread::Stopped Must Be Emitted for Every Turn

**File:** `crates/acp_thread/src/acp_thread.rs`

**Invariant:** Every call to `AcpThread::send()` must eventually emit exactly one `AcpThreadEvent::Stopped`, even if a second `send()` displaces the first turn mid-stream. Helix uses `message_completed` (triggered by `Stopped`) to pop its FIFO queue and route the next response. Missing a `Stopped` stalls the queue permanently.

**Context:** This is an upstream invariant that must hold across merges. If upstream changes `AcpThread::send()` to cancel in-progress turns without emitting `Stopped`, all subsequent Helix messages will stall.

**Test:** `test_second_send_during_active_turn_emits_stopped_for_both_turns` in `acp_thread.rs` verifies this invariant. Run it after every upstream merge: `cargo test -p acp_thread test_second_send`.

**History:** Documented in `8b033a4451`.

**Symptom:** Follow-up messages from Helix queue up but never get responses — Zed appears to process only the first message then goes silent.

### 7. THREAD_REGISTRY Must Be Unregistered on Entity Replacement

**File:** `crates/agent_ui/src/acp/thread_view.rs`

**Bug:** After a container restart, `load_thread_from_agent()` creates a **new** `Entity<AcpThread>` for the same session ID. If the old entity is still registered in `THREAD_REGISTRY`, thread_service will send follow-up messages to the stale entity, which no longer receives live events. The agent panel observes the new entity (live), but Helix receives updates from the dead entity — causing "brain split" where Zed is working but Helix sees nothing.

**Fix:** When `thread_view.rs` detects the displayed thread entity has changed (comparing by `EntityId`, not session ID), call `external_websocket_sync::unregister_thread()` before rebinding. The new entity registration happens automatically when thread_service re-registers it.

**History:** Added in `87632d00ce`. Detected by checking `active_thread.read(cx).thread == notification.thread_entity` in the `ThreadDisplayNotification` handler.

**Symptom:** After container restart, Zed works fine locally but all Helix messages are silently swallowed — no responses appear in the Helix session.

### 8. Cancel Must Drop send_task, Not Await It

**File:** `crates/acp_thread/src/acp_thread.rs` — `AcpThread::cancel()`

**Bug:** `cancel()` called `cx.background_spawn(turn.send_task)` to wait for the old turn's prompt future to complete before starting the next turn. This required the ACP agent to properly respond to `CancelNotification`. Claude Code's `claude-agent-acp` has multiple bugs where cancel doesn't cause the prompt to return (see [#442](https://github.com/zed-industries/claude-agent-acp/issues/442), [#423](https://github.com/zed-industries/claude-agent-acp/pull/423)), causing `cancel()` to block indefinitely and the next turn to never start.

**Fix:** `drop(turn.send_task)` instead of awaiting it. Dropping the GPUI Task cancels the prompt future, which drops the oneshot `tx`. The `rx.await` in `run_turn` then returns `Err`, hitting the existing "tx dropped" handler that emits `Stopped(Cancelled)`. The `connection.cancel()` notification is still sent as a courtesy, but we don't wait for acknowledgement.

```rust
pub fn cancel(&mut self, cx: &mut Context<Self>) -> Task<()> {
    let Some(turn) = self.running_turn.take() else {
        return Task::ready(());
    };
    self.connection.cancel(&self.session_id, cx);
    Self::flush_streaming_text(&mut self.streaming_text_buffer, cx);
    self.mark_pending_tools_as_canceled();

    // Drop instead of: cx.background_spawn(turn.send_task)
    drop(turn.send_task);
    Task::ready(())
}
```

**History:** Fixed in `6e0e6db32b`. The previous approach (`cx.background_spawn`) worked for NativeAgent (which responds to cancel immediately) but deadlocked with Claude Code.

**Symptom:** Phase 8 (mid-stream interrupt) times out for Claude Code agent. User pressing stop/interrupt in Zed while Claude Code is streaming causes the thread to hang permanently.

**Note:** Even if the claude-agent-acp cancel bugs (#442, #423) are fully fixed upstream, the drop approach should be kept as a defensive measure. Any ACP agent that doesn't properly respond to `CancelNotification` would cause the same deadlock. The drop approach makes Zed resilient to buggy agent implementations without changing protocol semantics (the cancel notification is still sent).

### 9. Guard Normal-Completion Stopped Against Duplicate Emission

**File:** `crates/acp_thread/src/acp_thread.rs` — `run_turn()` outer future

**Bug:** Critical Fix #8 made `cancel()` emit `Stopped(Cancelled)` synchronously and set `stopped_emitted` to prevent the Err/tx-dropped path from re-emitting. However, if `tx.send(Ok(response))` races ahead of the task drop (the prompt completes naturally just before the cancel takes effect), `rx.await` returns `Ok` and enters the **normal completion path** — which emits `Stopped` at line ~2290 without checking `stopped_emitted`. This causes a duplicate `Stopped` event.

The duplicate Stopped triggers the thread_service subscription's stale-detection logic (`turn_request_id == last_completed_request_id`), which falls back to the global `THREAD_REQUEST_MAP` — now pointing to the NEXT turn's request_id. The second `message_completed` is sent with the next turn's request_id, **prematurely completing the next interaction** and shifting all subsequent responses off by one.

**Fix:** Add the same `stopped_emitted_for_task` guard to the normal completion path:

```rust
// In run_turn(), Ok branch, before emitting Stopped:
if !stopped_emitted_for_task.load(std::sync::atomic::Ordering::Acquire) {
    cx.emit(AcpThreadEvent::Stopped(r.stop_reason));
}
```

**History:** Detected via container logs showing 3 Stopped events for a single interaction, causing systematic n-1 response shift in a localhost session.

**Symptom:** After an interrupt, all subsequent messages return the response for the _previous_ message. The session appears permanently "off by one."

### 10. ~~Bump Context-Server Request Timeout to 180s~~ (RETIRED 2026-05-21 — commit `e60a1b2789`)

**Status:** **Reverted.** The 180s bump was based on a wrong diagnosis (npm cold-start headroom). The real failure was `npx -y <pkg>` finding `<pkg>` already on PATH — npm shells out and exits, breaking stdio for the MCP client tracking the spawned PID. The MCP child died in <1s in that case; 180s just delayed the failure surfacing by ~3 minutes and made it harder to notice. With MCP configs fixed to invoke installed binaries directly (the correct pattern already used by chrome-devtools and the frontend presets), upstream's 60s is ample headroom for genuine cold-starts and lets real failures surface fast.

**Action for future merges:** none — the fix has been retired. Do NOT re-apply the 180s bump. `DEFAULT_REQUEST_TIMEOUT` should match upstream.

### 11. Entity-Identity Guard at the Top of `load_agent_thread`

**File:** `crates/agent_ui/src/agent_panel.rs` — first block inside `pub fn load_agent_thread`, gated on `#[cfg(feature = "external_websocket_sync")]`.

**Bug:** The new agents sidebar (added in PR #42 / PR #43) routes user clicks through `panel.load_agent_thread(session_id)`. That function dedups against the active / retained `ConversationView`s by comparing `cv.root_session_id == session_id`. In Helix mode, threads are first brought into the panel by `notify_thread_display` → `ConversationView::from_existing_thread(entity)`, which uses **entity-id** identity, not session-id identity. If `root_session_id` ever desyncs from the live `Entity<AcpThread>` registered in `external_websocket_sync::THREAD_REGISTRY`, the session-id check misses, `external_thread` runs, `connection.load_session()` is issued a second time, and a fresh `Entity<AcpThread>` Y is bound to the panel. The Helix WebSocket subscription stays on the original entity X (events keep flowing to the Helix server) but the panel is now bound to Y (silent) — split-brain.

**Fix:** Before any `has_session` check, look up `external_websocket_sync::get_thread(session_id)`. If a live entity exists, use **entity-id** comparison to (a) detect a no-op, (b) promote a retained CV that observes it, or (c) wrap it via `from_existing_thread` if no CV observes it. This is the same identity check `notify_thread_display` already uses, just applied at the UI entry point so both paths agree.

**Why it must run before the upstream `has_session` block:** The upstream block is correct for pure-Zed sessions but cannot be modified without diverging from upstream. The new guard wraps it so the Helix-mode invariant — *one live `Entity<AcpThread>` per session, observed by exactly one active CV* — holds without touching upstream code.

**Rebase checklist additions:**
- When `agent_panel.rs::load_agent_thread` is touched upstream, **re-check the new guard sits above the existing `has_session` block**. If upstream restructures the function, the guard must be re-applied; otherwise the bug returns silently.
- The guard depends on `external_websocket_sync::get_thread`, `register_thread`, and `THREAD_REGISTRY`. If those APIs are renamed in `crates/external_websocket_sync/src/thread_service.rs`, update the guard.
- Related fix (Critical Fix not numbered, commit `d7be64fad1`): `unregister_thread_if_matches` makes `cx.on_release` safe. Both fixes are required — they cover restart and interactive paths respectively. Don't remove either while the other still exists.

**History:** Detected after PRs #42/#43 landed the new sidebar. User-reported: "if I click the currently-open thread in the new sidebar, it stops updating in Zed but Helix still receives events." Spec task `001913_after-merging-latest-2`.

## Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `ZED_EXTERNAL_SYNC_ENABLED` | Enable WebSocket sync | `false` |
| `ZED_HELIX_URL` | Helix API server URL (host:port) | none |
| `ZED_HELIX_TOKEN` | Auth token for WebSocket | none |
| `ZED_HELIX_TLS` | Use TLS for WebSocket | `true` |
| `ZED_HELIX_SKIP_TLS_VERIFY` | Skip TLS cert verification | `false` |
| `ZED_HTTP_INSECURE_TLS` | Skip TLS for all HTTP (enterprise) | `0` |
| `ZED_WORK_DIR` | Working directory for sessions | auto-detected |
| `ZED_STATELESS` | Don't persist thread state | not set |
| `HELIX_SESSION_ID` | Session ID the headless WebSocket client identifies as (required when running `zed --headless`) | none |

## Headless Mode

`zed --headless` runs the binary with no display server (Wayland/X11) and no windows.
It still:
- Initializes `external_websocket_sync` and connects to Helix (when settings enable it).
- Wires up the thread service handler over a windowless `Project::local`.
- Runs the GPUI event loop until interrupted, so MCP/agent turns can stream as normal.

What it deliberately skips:
- `restore_or_create_workspace` — no `MultiWorkspace`/`Workspace`/`Window` is ever opened.
- `OpenListener` second-instance handling (`--headless` implies `--allow-multiple-instances`).

Use cases: running Zed as a Helix agent backend on a server with no GUI; running inside a
container that doesn't have Sway/Hyprland/X11; smoke-testing the WebSocket protocol without
a desktop.

Quickstart:
```bash
ZED_EXTERNAL_SYNC_ENABLED=1 \
ZED_HELIX_URL=api.example.com:8080 \
ZED_HELIX_TOKEN=... \
HELIX_SESSION_ID=ses_... \
zed --headless --user-data-dir /var/lib/zed-agent
```

### Headless mode and the E2E test

The E2E test runner supports `E2E_HEADLESS=1` which skips Xvfb, unsets
`DISPLAY`, and launches Zed with `--headless`:

```bash
E2E_HEADLESS=1 ./run_docker_e2e.sh --no-build
```

All 12 phases pass in headless mode for both `zed-agent` and `claude`. Phase 6
(`query_ui_state`) works because `initialize_headless()` registers a synthetic
UI-state responder that returns `active_view: "headless"` plus the real
`mcp_servers` map read from the headless project's `context_server_store`.
`thread_id`, `entry_count`, and `active_model` are reported as null/0 (the
panel is the source of those in headful mode and we don't track them here).

**Recommended CI matrix** — covers both agents and both display modes without
spending more wall-clock than the original single-mode default:

| Job | Command | Runs | Time |
|-----|---------|------|------|
| `e2e-headful` | `./run_docker_e2e.sh` | zed-agent (headful, full 12 phases) | ~3–4 min |
| `e2e-headless` | `E2E_HEADLESS=1 E2E_AGENTS=claude ./run_docker_e2e.sh` | claude (headless, full 12 phases) | ~3–4 min |

Run them as parallel CI jobs and total wall clock stays at ~3–4 min. If you
want to test both agents in both modes, expand to a 4-cell matrix. The
`E2E_AGENTS` and `E2E_HEADLESS` env vars compose freely.

## Callback Architecture

The WebSocket sync layer communicates with the agent panel via global callback channels (using `tokio::sync::mpsc`). This avoids tight coupling:

```
WebSocket message received
    → websocket_sync.rs: dispatches by command type
    → thread_service.rs: processes via MPSC channel
    → external_websocket_sync.rs: calls global callback (e.g., notify_thread_display)
    → agent_panel.rs: callback handler updates UI
```

Global callbacks initialized during agent panel setup:
- `GLOBAL_THREAD_CREATION_CALLBACK` — create new thread or follow up
- `GLOBAL_THREAD_DISPLAY_CALLBACK` — display a thread in agent panel
- `GLOBAL_THREAD_OPEN_CALLBACK` — open existing thread from agent
- `GLOBAL_UI_STATE_QUERY_CALLBACK` — query current UI state

Pending request queues (`PENDING_*`) buffer requests that arrive before callbacks are registered.

## Rebase Checklist

When rebasing/merging against upstream Zed:

1. **Preserve the `external_websocket_sync` crate** — it's self-contained and rarely conflicts
2. **Check `agent.rs` `load_session()`** — ensure the entity lifetime fix is present (Critical Fix #1)
3. **Check `thread_view.rs` event handlers** — ensure no duplicate WebSocket sends (Critical Fix #2)
4. **Check `acp_thread.rs` `AssistantMessage`** — ensure `content_only()` exists (Critical Fix #3)
5. **Check `thread_service.rs` follow-up path** — ensure `notify_thread_display()` is called (Critical Fix #4)
6. **Check `thread_service.rs` streaming path** — ensure stale pending entries are flushed when a new entry starts (Critical Fix #5)
7. **Run `cargo test -p acp_thread test_second_send`** — verifies `Stopped` invariant (Critical Fix #6)
8. **Check `thread_view.rs` unregistration** — ensure `unregister_thread()` is called when entity changes (Critical Fix #7)
9. **Check `agent_panel.rs` cfg-gated blocks** — callback setup, `from_existing_thread`, onboarding dismissal, `acp_history_store()`, entity-level split-brain detection, auto-follow activation
10. **Check `conversation_view.rs` cfg-gated blocks** — `HeadlessConnection` (needs `agent_id()` + correct `new_session(PathList)` signature), `from_existing_thread()`, THREAD_REGISTRY registration, `self.history()` method call (not field), `is_resume = load_session_id.is_some()` (not `resume_thread`), `unregister_thread()` on reset
11. **Check `from_existing_thread()` matches `ConnectedServerState` struct** — upstream may change required fields (currently: `connection`, `auth_state`, `active_id`, `threads` HashMap, `conversation` Entity, `history`, `_connection_entry_subscription`). `ConversationView` itself also requires `connection_store` and `connection_key` fields (no `login`/`history` direct fields).
12. **Check `connection.rs` `AgentConnection` trait** — if upstream added new methods, `HeadlessConnection` must implement them. Currently requires `agent_id()` and `new_session(project, PathList, cx)`. Check for compilation errors.
12a. **Check `AcpThreadEvent::Stopped` usage** — it's a tuple variant `Stopped(StopReason)`. Pattern matches must use `Stopped(_)`, emissions must pass a reason e.g. `AcpThreadEvent::Stopped(acp::StopReason::Cancelled)`.
13. **Check `thread_service.rs` uses new `AgentServer`/`AgentConnection` APIs** — `AgentServerDelegate::new(store, None)` (2 args), `server.connect(delegate, project, cx)` (3 args, returns `Rc` not tuple), `connection.new_session(project, PathList::new(&[cwd]), cx)`, `connection.load_session(session_id, project, PathList::new(&[cwd]), None, cx)` (5 args), `first_method.id()` (method not field)
14. **Check `types.rs` `ExternalAgent::server()` uses `CustomAgentServer::new(AgentId(...))`** — `Gemini`/`ClaudeCode` structs removed from `agent_servers`
15. **Check `workspace.rs` `follow()` and `update_follower_items()`** — `CollaboratorId::Agent` must not steal keyboard focus (no `window.focus()` call for Agent leader)
16. **Check `migrate.rs`** — migration banner returns `Hidden` in Helix builds
17. **Check `grep_tool.rs`** — `truncate_long_lines()` and `MAX_LINE_CHARS = 500` present
18. **Check `config_options.rs`** — `current_model_value()` method present
19. **Check `conversation_view/thread_view.rs` `current_model_id()`** — three-way fallback (selector → config_options → global registry)
20. **Check `extensions_ui.rs`** — agent keyword/upsell removal preserved
21. **Check `dev_container_suggest.rs`** — `suggest_dev_container` early return preserved
22. **Check `feature_flags/flags.rs`** — `AcpBetaFeatureFlag::enabled_for_all()` returns `true`
23. **Check `http_client_tls.rs`** — `NoCertVerifier` and `ZED_HTTP_INSECURE_TLS` support
24. **Check `reqwest_client.rs`** — insecure TLS support
25. **Check `title_bar`** — connection status indicator + `external_websocket_sync` dependency
26. **Check `agent_settings`** — `show_onboarding`, `auto_open_panel` fields
27. **Check `.dockerignore`** — simplified for Helix builds
28. **Check `SyncEvent::MessageAdded`** — has `entry_type`, `tool_name`, `tool_status` fields
29. **Check `SyncEvent::UiStateResponse`** — has `mcp_servers` and `active_model` fields
30. **Check `NativeAgent` multi-project**: `agent.projects.values().next()` to get `ProjectState`; no more flat `agent.project` or `agent.context_server_registry()` fields/methods
31. **Check `acp_thread.rs` `cancel()`** — must `drop(turn.send_task)` not `cx.background_spawn(turn.send_task)` (Critical Fix #8)
31a. **Check `acp_thread.rs` `run_turn()` normal completion** — `Stopped` emission must be guarded by `stopped_emitted_for_task` check (Critical Fix #9)
34. **Check `agent_panel.rs` `request_permission()`** — when `external_websocket_sync` is enabled, auto-select first AllowOnce option and return immediately (ACP auto-approve for autonomous mode)
35. **Check `agent_panel.rs` agent_type serialization** — correct agent_type must be serialized for externally-opened threads and panel restoration
36. **Check `thread_service.rs` turn-scoped request_id** — EntryUpdated uses turn-scoped request_id with prev_turn fallback; NewEntry updates turn_request_id only at turn boundaries
37. **Check `acp_thread.rs` `run_turn()` stopped_emitted_for_task** — normal completion Stopped must check stopped_emitted_for_task to prevent duplicate emission racing with cancel()
38. **Check trial-end upsell guard** — `suggest_trial_end_upsell()` returns early in Helix builds
39. **Check `crates/zed/src/main.rs` for `--allow-multiple-instances` CLI flag** — defined as `#[arg(long)] allow_multiple_instances: bool` on `Args`, AND used in the `failed_single_instance_check` short-circuit (`|| args.allow_multiple_instances`). This Helix-only flag was lost in the 001864 merge (re-added by 001909). Without it the e2e-test container can't launch Zed at all.
39a. **Check `crates/zed/src/main.rs` for `--headless` CLI flag** — defined as `#[arg(long)] headless: bool` on `Args`. Three call sites must all be present:
   1. `Application::with_platform(gpui_platform::current_platform(args.headless))` — passes the bool into GPUI so Linux uses `HeadlessClient` instead of trying X11/Wayland.
   2. `failed_single_instance_check` `|| args.headless` short-circuit — singleton lock must not block the headless backend.
   3. Inside the `app.run` closure, after `cx.activate(true)` and the `authenticate` spawn, an `if args.headless { initialize_headless(app_state.clone(), cx); return; }` branch that skips workspace/window creation entirely. The `initialize_headless()` function (cfg-gated on `external_websocket_sync`) builds a `Project::local` with no worktrees, grabs `ThreadStore::global(cx)`, calls `external_websocket_sync::setup_thread_handler(...)`, and then starts the sync via `init_websocket_service(WebSocketSyncConfig{...})` when settings enable it.
   Without these the binary still has the flag but quietly opens windows / dies on `open_window` failure.
40. **Check `Cargo.toml` workspace `rust-embed` features** — must include both `include-exclude` AND `debug-embed`. The `debug-embed` feature was originally added by Helix in commit `9ca797706f` (Oct 2025), lost in a subsequent merge, re-added in 001909. Without it, dev builds panic on startup with `settings/default.json` because RustEmbed tries to read assets from `CARGO_MANIFEST_DIR` at runtime, and that path doesn't exist outside the build directory (e.g. inside the e2e-test container or any deployed binary). Release builds always embed assets so they're unaffected — but debug builds (used by the e2e test, ARM aside) need this feature.
41. **Check `crates/agent/src/agent.rs` for `smol::Timer::after` references** — must use `cx.background_executor().timer(d).await` instead. Upstream PR #53603 (Apr 2026) removed `smol` from the agent crate's deps. Helix's `wait_for_tools_ready()` previously used `smol::Timer::after` and broke after the merge; fixed in 001909 by switching to the canonical GPUI pattern.
41a. **Check `acp_thread.rs` test code for unit-variant `AcpThreadEvent::Stopped` patterns** — `Stopped` is a tuple variant `Stopped(StopReason)` and `matches!(event, AcpThreadEvent::Stopped)` no longer compiles. Production builds skip `#[cfg(test)]` so this fails silently in `cargo build` but breaks `cargo test -p acp_thread test_second_send`. Grep: `grep -n "AcpThreadEvent::Stopped[^(]" crates/acp_thread/src/`. Fixed in 001980; patterns must be `Stopped(_)`.
42. **Run `cargo check --package zed --features external_websocket_sync`** — must compile
43. **Run `cargo test -p external_websocket_sync`** — unit tests
44. **Run E2E test** after merge to verify all phases pass (currently 12 phases, run for both `zed-agent` and `claude` rounds)

## Building

The recommended way to build and test is via the `stack` command in the Helix repo (`~/pm/helix/stack` or `~/work/helix/stack`), which handles Docker-based compilation with persistent caching:

```bash
# Build Zed binary (dev mode, ~3 min with warm cache)
cd ~/pm/helix   # or ~/work/helix
./stack build-zed dev

# Build Zed binary (release mode, ~12 min)
./stack build-zed release

# Output: ./zed-build/zed
```

For running E2E tests, build the binary first then copy it into the test directory:

```bash
# Build + run E2E tests
cd ~/pm/helix && ./stack build-zed dev
cp ~/pm/helix/zed-build/zed ~/pm/zed/crates/external_websocket_sync/e2e-test/zed-binary
cd ~/pm/zed/crates/external_websocket_sync/e2e-test
./run_docker_e2e.sh                                # zed-agent only
E2E_AGENTS="zed-agent,claude" ./run_docker_e2e.sh  # both agents
```

Direct cargo commands also work if you have a Rust toolchain installed locally:

```bash
# Build with Helix features
cargo build --features external_websocket_sync -p zed

# Run unit tests
cargo test -p external_websocket_sync

# Run E2E test via Docker directly (alternative to stack)
docker build -t zed-ws-e2e -f crates/external_websocket_sync/e2e-test/Dockerfile .
docker run --rm -e ANTHROPIC_API_KEY=sk-ant-... -e TEST_TIMEOUT=120 zed-ws-e2e
```

## Commit History

Helix-specific commits on main (oldest first):

| Commit | Description |
|--------|-------------|
| `4cae6d9` | Port Helix fork changes to fresh upstream Zed |
| `54296a7` | Add WebSocket protocol spec, mock server, and test infrastructure |
| `b063ae0` | Add E2E test infrastructure with Docker container |
| `463b1cc` | Fix E2E test infrastructure: Docker caching, headless Zed startup |
| `bc52393` | Fix model configuration race and E2E test settings |
| `5fe75be` | Fix WebSocket event forwarding for thread_service-created threads |
| `746a9c4` | Add multi-thread E2E test: follow-ups and thread transitions |
| `7da861b` | Simplify .dockerignore for helix build context |
| `6fd8116` | Update Cargo.lock for agent_settings dependency |
| `cf72593` | Restore thread auto-open and disable restricted mode |
| `e0cc99f` | Implement from_existing_thread for AcpServerView |
| `a83ddc0` | Add query_ui_state command for E2E UI verification |
| `cc037db` | Send WebSocket events from thread_service instead of UI subscription |
| `55882e8` | Fix UI freeze and thread_id mismatch in from_existing_thread |
| `01c0c11` | Streaming WebSocket events, thread persistence, dismiss onboarding |
| `3ae2f1e` | Hide built-in agents (Claude Code, Codex, Gemini) in Helix builds |
| `4e87001` | Enable ACP beta features for session list and resume |
| `fb96f34` | Add Phase 4 E2E test + fix follow-up to non-visible thread |
| `0a78bf8` | **Fix: keep NativeAgent entity alive during load_session** |
| `98ec442` | **Fix: strip '## Assistant' heading from WebSocket-synced messages** |
| `72e2952` | **Fix: remove duplicate WebSocket event sends from thread_view.rs** |
| `818cf940e6` | Fix: adapt external_websocket_sync to upstream connect() API change |
| `0b9e2211dc` | Fix: wire up auto_open_panel setting to AgentPanel starts_open() |
| `2f74e89657` | Fix: disable migration banner in Helix builds (`migrate.rs`) |
| `f51c0d5dae` | Truncate long lines in grep tool output (500 char limit) |
| `1fab62117e` | Prevent keyboard focus stealing when following agent (`workspace.rs`) |
| `87632d00ce` | **Fix: thread entity split-brain after container restart (unregister on entity change)** |
| `3e4d7d7bbc` | Fix: wait for MCP tools to load before sending first WebSocket message |
| `d511c3e983` | Add Dockerfile.ci for E2E tests in CI |
| `e42b1ad95e` | Fix auto-follow mode and split-brain for external WebSocket sessions |
| `29f10aa7ad` | Emit MessageCompleted from Stopped event for all turn sources |
| `182cae0ead` | Fix missing message_completed in follow-up subscription |
| `91c281fb93` | Extract ensure_thread_subscription to fix missing event handlers |
| `c33ee0483b` | Add entry_type field to MessageAdded sync event |
| `1e66f0ada2` | Add ResponseEntries validation to E2E test |
| `6e4967240a` | **Fix: flush stale pending entries when different entry starts streaming** |
| `4e204c4d7d` | Handle ToolCall in NewEntry event (not just EntryUpdated) |
| `bfe84a2134` | Send structured tool_name and tool_status in message_added events |
| `e38aad1a18` | Clear persistent subscription on unregister to fix E2E timeout |
| `8b033a4451` | **Test: add Stopped emission and mid-stream interrupt E2E tests (Critical Fix #6)** |
| `85be6df7b6` | **Fix: E2E seed session, user_created_thread tracking, interaction count** |
| `6e0e6db32b` | **Fix: drop cancel task to prevent deadlock with Claude Code (Critical Fix #8)** |
| `71fb5fba73` | Fix: use correct Agent for claude-acp threads in agent_panel |
| `1a3fc57adc` | Add request_id to message_added events for interaction routing |
| `255f6b4522` | Fix Stopped flush: use turn-scoped request_id, only flush current turn |
| `bc4921681f` | Scope NewEntry re-send to current turn to prevent cross-interaction leaks |
| `6e35959201` | Remove streaming text reveal rate-limit to fix WebSocket sync truncation |
| `73f9af2162` | Re-read current entry content in NewEntry handler instead of flushing stale pending |
| `14c079c266` | Flush pending text content before sending new entries to prevent truncated streaming |
| `520f327183` | Fix: serialize correct agent_type for externally-opened threads |
| `cf4e7d6f78` | Fix: serialize agent_type + wait for WebSocket before panel restoration |
| `f3a2622736` | Fix: send agent_ready and set up subscription from panel restoration path |
| `48de0cf877` | Fix: share thread load lock with panel restoration, use agent_id for agent_ready |
| `0fef8b27c1` | Fix: send agent_ready even when no thread to restore |
| `d470dac687` | Fix: coordinate panel restoration and open_existing_thread_sync via load lock |
| `47950a9cf8` | Fix: call ensure_thread_subscription in open_existing_thread_sync |
| `90bdb6cf75` | Fix: emit Stopped synchronously in cancel() to fix phase 8 FIFO ordering |
| `a7e4d8b850` | Fix: implement real interrupt — cancel running turn before queuing new message |
| `2f182e64d6` | **Fix: prevent request_id desync from background events and duplicate Stopped (Critical Fix #9)** |
| `f96525f558` | Fix: filter stale phase completions in E2E test |
| `55f797f2bc` | **Auto-approve ACP permission requests when external_websocket_sync is enabled** |
| `9f0475c6c2` | fix: drop stale display_name reference in [ACP_SPAWN] log |
| `d7be64fad1` | **fix: stop empty message_completed loop after Zed restart + Helix-mode UI cleanup** |
| `8428a4399d` | Merge upstream Zed (62bd61a679..e3d1876c06, 86 commits) into 001909-merge-latest-zed |
| `6ccf3010a6` | **Fix `wait_for_tools_ready`: use `cx.background_executor().timer()` instead of `smol::Timer` (upstream PR #53603 dropped smol)** |
| `16f2b82053` | **Restore `--allow-multiple-instances` CLI flag (lost in 001864 merge)** |
| `c7a26c9144` | **Restore `debug-embed` feature on `rust-embed` workspace dep (lost in a prior merge — required for dev/debug builds outside source tree)** |
| `3cfc2962d1` | Merge `origin/main` into 001909 (incorporates `d7be64fad1`) |
| `c3e312b056` | Merge upstream Zed (`8428a4399d..1da60a8518`, 172 commits, 10 days) into 001980 — 4 conflicts resolved (`deploy_cloudflare.yml`, `Cargo.lock`, `agent_settings.rs`, `wgpu_renderer.rs`) |
| `95715a1798` | **Fix `AcpThreadEvent::Stopped` test patterns: tuple variant requires `Stopped(_)` (pre-existing breakage since 001864 — never noticed because `#[cfg(test)]` skipped in production builds)** |
| `61427db325` | Tidy e2e test server `go.mod` for current helix deps (`kodit v1.3.6 → v1.3.7`, dropped `go-tika`) — runner doesn't tidy itself |
| `bf544922aa` | Merge upstream Zed (`1da60a8518..8bdd78e023`, 127 commits, 3 days) into 001996 — 1 conflict resolved (`acp_thread.rs` cancel/Stopped path; folded upstream PR #55562 reorder with Helix Critical Fixes #6/#8/#9 dropped-tx Stopped emission) |
| `1828cea13c` | Fix: handle `BaseView::Terminal` in Helix UI state query (upstream added the variant; the cfg-gated match in `agent_panel.rs:1270` was non-exhaustive — caught by build, not by silent-drift sweep) |
| `a7ad11ec00` | Fix Phase 13 race: cancel handler now probes `thread.status()` and sends `turn_cancelled` BEFORE calling `cancel()` so Helix marks the interaction as Interrupted before message_completed (triggered by the synchronously-emitted Stopped) arrives and races it into Completed — discovered by E2E Phase 13 failing on the first run |
| `6b39672e5f` | Merge upstream Zed (`8bdd78e023..1399540715`, 261 commits, 10 days) into 002029 — 6 conflicts resolved: workflows (theirs), title_bar Cargo.toml (kept Helix external_websocket_sync dep, dropped feature_flags), title_bar.rs `render_restricted_mode` (kept Helix early-return + adopted upstream's free-function API), agent_server_store.rs reregister_agents destructure (dropped `extension_agents`, kept `_subscriptions`/`registry_subscribed`, added `..`), agent_panel.rs load_panel restoration (kept Helix WS-wait + send_agent_ready, adopted upstream thread_to_restore + load_agent_thread + restore_new_draft), agent_panel.rs load_agent_thread (adapted Critical Fix #11 entity-identity guard to upstream's thread_id signature via ThreadMetadataStore session_id lookup), agent_panel.rs ensure_thread_initialized (Helix Fix 1b early-return as FIRST statement, before upstream 589dc95c87's new terminal-spawn branches) |
| `edbc05cf99` | Build fixes for upstream signature drift: agent_servers/acp.rs PR #50 chain log-labels now use `directories.cwd` (upstream c3951af24f removed local `cwd` binding); agent_ui/conversation_view.rs from_existing_thread adapted to new ThreadView::new signature (root_thread_id first arg), 3-arg SessionCapabilities::new, and new ConversationView fields (draft_prompt_persist_task, last_theme_id); agent_ui/agent_panel.rs + zed/main.rs added ContextServerStatus::ClientSecretRequired arm |
| `b2993c0b01` | Merge upstream Zed (`9d50bab893..992f395c3d`, 256 commits, 10 days) into 002077 — 6 conflicts resolved (2 workflows deleted `git rm`, 1 workflow `--theirs`, language_model/model/mod.rs accepted upstream deletion of entire `model/` directory, recent_projects/dev_container_suggest.rs kept Helix `Settings` import + upstream `std::path::Path` import, title_bar/title_bar.rs kept Helix Plan+external_websocket_sync imports + added upstream `CommandPaletteFilter`). Helix surface auto-merged cleanly across all critical fixes — no PR #55 emit relocation needed, no Fix 1b movement needed, no `Workspace::show_error` call sites in Helix surface to migrate. Build green on first try. |
| `0098823efa` | Merge upstream Zed (`992f395c3d..a31d3505da`, 25 commits, 3 days) into 002100 — 1 conflict resolved (`settings_content/src/settings_content.rs` both-sides-added-a-field on `RemoteSettingsContent`: kept Helix `suggest_dev_container` + upstream `dev_container_use_buildkit`). Smallest catch-up window in the series. Zero upstream churn in `acp_thread/`, `agent/src/`, `workspace.rs`, `zed/src/main.rs`, `title_bar/`, `feature_flags/`, `agent_servers/`, `external_websocket_sync/`, `agent_settings/` — all critical fixes and PR #50/#55/#56/#60 surface intact by construction. |
| `0e0149ade5` | Merge upstream Zed (`a31d3505da..e45e42af6e`, 95 commits, 3 days) into 002100-extension — 1 conflict resolved (`agent/src/tools/grep_tool.rs`: kept Helix 001410 `truncate_long_lines` semantic while reusing upstream's pre-computed `snippet` variable from `40211567b8` "Make grep tool results clickable in agent panel"). Heavy upstream churn in `acp_thread.rs` (+198), `agent.rs` (+223), `agent/src/thread.rs` (+511), `agent_panel.rs` (+203), `conversation_view.rs` (+1024), new `thread_search_bar.rs` (+962), `thread_view.rs` (+1094), `extensions_ui.rs` (+286), `title_bar.rs` (+36), `agent/src/sandboxing.rs` (+458), `agent/src/tools/terminal_tool.rs` (+957) — yet all auto-merged cleanly. Fix 1b shifted from line 5420 → 5468 (still FIRST statement of `BaseView::Uninitialized`). Three `// HELIX:` markers shifted from 226/248/1518 → 337/359/1629. Critical Fix #3 shifted from line 262 → 335. All shifts content-preserving. |
| `2221360fc1` | Tidy ws-test-server `go.mod`/`go.sum` for current Helix deps (certmagic / libdns / acmez / miekg/dns). Same pattern as `9f8364e138` after round 1. |

## Merge 002224 (2026-07-06)

**Divergence at start**:
- Branch HEAD (fork main): `9546054e68` (PR #65 — emit terminal frame when ACP agent crashes mid-turn).
- Last upstream merge fence: `e45e42af6e` ("agent_ui: Use the thread title for agent notifications (#59377)") — absorbed in 002100-extension; confirmed as the `git merge-base main upstream/main`.
- Upstream HEAD: `872ca8fef5` ("Add license symlinks to lint test fixture crates (#60505)").
- **Upstream commits to merge: 289** (18-day window, 2026-06-18 → 2026-07-06). Largest single window since 002029.
- Helix-only commits since 002100-extension: **0** (fork main was exactly at PR #65).
- **Major dependency bump**: `agent-client-protocol` **0.14.0 → 1.0.1** (and `-schema` 0.13.6 → 1.x). This is the dominant risk of the window — the ACP builder API and `send_request` execution changed.

Merge strategy: `git merge upstream/main` (no rebase), per every prior merge. Merge commit `70bf514b87`.

### Conflicts and Resolutions

`git merge` produced **5 content conflicts + 2 workflow modify/deletes**. Despite enormous churn in the touched files (`acp_thread.rs` +3707, `agent_servers/src/acp.rs` +1232, `agent_panel.rs` 824 changed, `connection.rs` 89, `zed.rs` 142), the Helix surface collided in only five spots.

#### 1. `.github/workflows/{hotfix-review-monitor,stale-pr-reminder}.yml` — modify/delete
**Resolution**: `git rm` both. Helix deleted these Zed-CI workflows; upstream modified them. Helix doesn't run Zed's GitHub CI. (Standard resolution, same as prior merges.)

#### 2. `crates/reqwest_client/src/reqwest_client.rs` — content
**Upstream change**: added HTTP/2 keep-alive + pool tuning to `builder()` (`tcp_keepalive`, `pool_idle_timeout`, `http2_keep_alive_*`) to drop silently-dead connections (BadRecordMac mitigation).
**HEAD change**: Helix insecure-TLS branch (`ZED_HTTP_INSECURE_TLS` → `use_preconfigured_tls(insecure_tls_config())` else `use_rustls_tls()`).
**Resolution**: kept both — apply upstream's keep-alive tuning to the base `builder`, then branch on `is_insecure_tls_enabled()` for the TLS backend. No behavior lost on either side.

#### 3. `crates/agent_ui/src/config_options.rs` — content
**Upstream change**: renamed `first_config_option_id(category)` → `first_config_option_id_matching(category, predicate)`.
**HEAD change**: Helix `current_model_value()` accessor (used by the model-id fallback in `thread_view`) called the old `first_config_option_id`.
**Resolution**: kept `current_model_value()`, rewrote its call to `first_config_option_id_matching(Model, |_| true)`. Took upstream's renamed helper.

#### 4. `crates/agent_ui/src/agent_panel.rs` — content (imports)
**Resolution**: pure both-added import block. Kept HEAD's Helix imports (`Agent`, `AgentInitialContent`, `AgentThreadSource`, `ExternalSourcePrompt`, `NewExternalAgentThread`, `NewNativeAgentThreadFromSummary`, the `external_websocket_sync_dep as external_websocket_sync` alias, and `tokio::sync::mpsc`), followed by upstream's `agent_settings::AgentSettings`.

#### 5. `crates/agent_servers/src/acp.rs` — content (×2)
**5a (both-added items)**: HEAD added PR #50's `SessionCreationGuard` struct + `Drop` impl; upstream added a new `client_capabilities_for_agent()` fn at the same spot. Kept both (they collided only by adjacency).
**5b (`new_session` send path)**: upstream replaced `into_foreground_future(connection.send_request(...))` with `connection.send_request(...).block_task()`. HEAD wrapped this in PR #50's session-creation slot (`let _slot_guard = slot_guard; prev_chain.await;`). **Resolution**: kept PR #50's slot-guard + `prev_chain.await`, switched the send to upstream's `.block_task()`. `load_session` (open_or_create_session) auto-merged and already uses `.block_task()` + the slot guard consistently. `into_foreground_future` remains used by the close-session path (line ~2004), so the import is not orphaned.

#### 6. `crates/acp_thread/src/acp_thread.rs` — content (×2)
Both hunks are the `run_turn` terminal path where upstream added `if is_same_turn { cx.emit(StatusChanged) }` and Helix has the Critical Fix #6/#9 `stopped_emitted_for_task` guard around `Stopped` emission.
**Resolution (tx-dropped branch, ~3800)**: kept Helix's guarded `Stopped(Cancelled)` emission, then added upstream's `is_same_turn → StatusChanged`.
**Resolution (normal-completion branch, ~3902)**: emit upstream's `StatusChanged` on same-turn first, then Helix's guarded `Stopped(r.stop_reason)` (guard preserved — Critical Fix #9).

### PR #65 Survival Check
- `StubAgentConnection::fail_turn()` intact — `acp_thread/src/connection.rs:936`.
- `SyncEvent::ChatResponseError` intact — `types.rs:230`; emit at `thread_service.rs:1106`.
- `TEST_WEBSOCKET_SERVICE_GUARD` present (3 refs in `thread_service.rs`, shared by crash + reconnect tests).

### Helix Surface — Auto-Merge Survival Check
All critical fixes intact post-merge (line numbers shifted by upstream churn):
- **Fix #1** `pending_sessions`/`load_session`: `agent/src/agent.rs:407/580/1616/2586`.
- **Fix #3** `content_only()`: `acp_thread.rs:339`.
- **Fix #6/#9** `stopped_emitted_for_task`: `acp_thread.rs:3762/3807/3911`.
- **Fix #8** `drop(turn.send_task)`: `acp_thread.rs:3969`.
- **Fix #11** entity-identity guard via `ThreadMetadataStore`: `agent_panel.rs:1911` (in `load_agent_thread`).
- **PR #50** `session_creation_chain` + `_settings_subscription`: `acp.rs:410/411`; slot held in both `new_session` and `load_session`.
- **PR #55** streaming `EntryUpdated` emit: 21 occurrences in `acp_thread.rs` (was ~16; upstream added sites, Helix's retained).
- **PR #56 Fix 1b** cfg-gated `return;` is the FIRST statement of the `BaseView::Uninitialized` branch in `ensure_thread_initialized`: `agent_panel.rs:5419-5424`.
- **PR #60** `ede_diagnostic` retry: `thread_service.rs:2025`.
- **PR #63/#64** wedge recovery + `agent_ready` re-emit: intact (Helix-only file, no upstream churn) — `thread_service.rs:1838/2277/2325`.
- **AcpBetaFeatureFlag::enabled_for_all()** override: `feature_flags/src/flags.rs:30`.
- **3× `// HELIX: External agent` markers**: `extensions_ui.rs:337/359/1629` (unshifted).
- **`render_restricted_mode`** cfg-gated early return: `title_bar.rs:699` (`cfg!(feature="external_websocket_sync") → None`).
- **`--allow-multiple-instances` / `--headless` / `initialize_headless` / `build_application(headless)`**: `zed/src/main.rs:86/343/361/362/886/1416`.
- **`rust-embed` `debug-embed` feature**: `Cargo.toml:741`.
- **No `smol::Timer` in `agent.rs`**: 0 hits.
- **No non-tuple `AcpThreadEvent::Stopped` patterns** in non-test code (only a comment at `acp_thread.rs:9542`).

### Finding — branding/trust settings are code-enforced, not in `default.json`
Task constraints listed `trust_all_worktrees: true` and `show_sign_in: false` as `assets/settings/default.json` invariants. **In this fork they are NOT set in `default.json`** — fork `main`, the merge base, and upstream all carry `show_sign_in: true` / `trust_all_worktrees: false`. Trust is enforced in code (`can_trust` auto-trusts every worktree; `render_restricted_mode` is cfg-gated to `None`); branding/sign-in is injected by the Helix platform at deploy time. No regression from this merge (values are byte-identical to fork `main`); no change made to `default.json`. Flag for the maintainer if the intent was to bake these into `default.json`.

### Cargo.toml / Cargo.lock notes
- `Cargo.lock` taken from upstream (`--theirs`); to be regenerated by the build. ACP `agent-client-protocol` now `1.0.1`.
- ACP 0.14→1.0 is a major bump; any Helix code using ACP struct literals / `ErrorCode` / `send_request` execution is compile-checked in the build stage (see below).

### Pre-existing Breakage Repaired (ACP 1.0 migration + upstream refactors)
The auto-merge left 5 categories of compile breakage in Helix-only code, all surfaced by `./stack build-zed dev --features external_websocket_sync` and fixed in dedicated commits. **Build is green with the feature after these.**

1. **`into_foreground_future` removed upstream** (`agent_servers/src/acp.rs`). Upstream migrated ACP request execution to `send_request(...).block_task()` and deleted the `into_foreground_future` helper. Two Helix sites used it: the PR #50 `new_session` slot path (fixed during conflict resolution) and the Helix close-session/keep-alive path at ~line 2004 (fixed post-build: `conn.send_request(CloseSessionRequest::new(session_id)).block_task().await?`).
2. **ACP schema moved under `v1`** (`external_websocket_sync/src/thread_service.rs:9`). ACP 1.0 relocated schema types into `agent_client_protocol::schema::v1`. Helix used `use agent_client_protocol::schema as acp;`; upstream code now uses `schema::v1 as acp`. Changed the Helix alias to `schema::v1 as acp` — fixes `acp::SessionId` (×3), `acp::ContentBlock`, `acp::TextContent`. This is the only ACP import in the whole crate.
3. **Duplicate imports in `agent_panel.rs`**. Upstream added `Agent, AgentInitialContent, AgentThreadSource, ExternalSourcePrompt, NewExternalAgentThread, NewNativeAgentThreadFromSummary` to its own `use crate::{…}` block; the Helix conflict block re-imported them → E0252 ×6. Removed the redundant Helix `use crate::{…}` block, keeping the Helix-only cfg imports (`external_websocket_sync_dep as external_websocket_sync`, `tokio::sync::mpsc`).
4. **Overlay/configuration-panel system removed upstream** (`agent_panel.rs`). `overlay_view`, `configuration`, `configuration_subscription` fields and `clear_overlay_state()` no longer exist. The Helix Fix #11 code in `load_agent_thread` called `self.clear_overlay_state()`; removed the now-obsolete call (the early-return + `ActiveViewChanged` emit is retained — the live thread is already displayed).
5. **`from_existing_thread` field-set drift — new ACP 1.0 elicitation fields** (`conversation_view.rs:1150`). Upstream added `request_elicitation_form_states: HashMap<…>` to `ConversationView` and `_request_elicitation_subscription: Option<Subscription>` to `ConnectedServerState`. Helix's `from_existing_thread()` constructor didn't set them → E0063 ×2. Added `request_elicitation_form_states: HashMap::default()` and `_request_elicitation_subscription: None`, matching `ConversationView::new` and the other `ConnectedServerState` initializers. (This is exactly the drift Rebase-Checklist item 11 warns about.)
6. **`agent::ThreadStore` import dropped** (`zed/src/main.rs`). Upstream's refactor removed the `use agent::{SharedThread, ThreadStore};` line (its own uses of `ThreadStore` went away); Helix's cfg-gated `initialize_headless()` still calls `ThreadStore::global(cx)`. Fully-qualified it to `agent::ThreadStore::global(cx)` (cfg-gated code, so inline qualification is cleaner than restoring an import).



**Divergence at start**:
- Branch HEAD: `4ae2094b54` (PR #64 — `agent_ready` re-emit on reopening already-loaded thread, landed on fork main between rounds) — built on top of round 1's `5ed995947e` plus PRs #63 (claude-agent-acp wedge recovery, 6 commits) and #64 (1 commit).
- Last upstream merge fence: `a31d3505da` ("git: Do not run `git stash list` on every file save (#59042)") — absorbed in round 1.
- Upstream HEAD: `e45e42af6e` ("agent_ui: Use the thread title for agent notifications (#59377)")
- Upstream commits to merge: **95** (3 days of dense activity since round 1; sandbox cluster, in-thread search, compaction-threshold tuning).
- Helix-only commits since round 1 (PR #63 + PR #64): **7** (`c295cbc697` force-reset on wedged Query, `f8e9d85d42` clear keep-alive + surface load_err on force-reset, `d78e1b4327` + `47104fbad0` diagnostic eprintlns, `9f8364e138` track agent_name per thread for wedge recovery, `be0dcd329e` strip dispatch diagnostics keep recovery signal, `4ae2094b54` emit agent_ready on reopening already-loaded thread)

This extension absorbs 95 upstream commits in 3 days — the inverse shape of round 1 (which absorbed 25 commits in 3 days). Total round 1 + round 2 = 120 upstream commits over 6 days.

### Conflicts and Resolutions

#### 1. `crates/agent/src/tools/grep_tool.rs` — content
**Upstream change**: PR `40211567b8` "Make grep tool results clickable in agent panel (#59230)" refactored the snippet handling — introduced `let snippet: String = snapshot.text_for_range(range.clone()).collect();` at line 325, reused `snippet` both for the markdown block and for the new `acp::ToolCallContent::Content(ContentBlock::Text(...))` clickable-resource emit downstream. Replaced the in-place `text` binding inside the markdown block with `snippet`.
**HEAD change**: Helix task 001410 `f51c0d5dae` "Truncate long lines in grep tool output to prevent context window blowups" wraps the markdown-block content with `truncate_long_lines(&text, MAX_LINE_CHARS)`.
**Resolution**: kept Helix's `truncate_long_lines` semantic, but rewrote the call to reuse upstream's pre-computed `snippet` variable: `output.push_str(&truncate_long_lines(&snippet, MAX_LINE_CHARS));`. Drops the duplicate `text` binding; the truncation behavior is unchanged.
**Risk**: none — `truncate_long_lines` operates on `&str`, both `text` and `snippet` carry identical content. The downstream `ContentBlock::Text(...)` still uses the un-truncated `snippet` (full content for clickable resource).

### Helix Surface — Auto-Merge Survival Check

All Helix critical fixes and load-bearing patches survived `git merge upstream/main` cleanly. The 95-commit window touched every Helix-adjacent surface (notably `acp_thread.rs` +198 lines, `agent/src/agent.rs` +223 lines, `agent/src/thread.rs` +511 lines, `agent_ui/src/agent_panel.rs` +203 lines, `agent_ui/src/conversation_view.rs` +1024 lines, new `agent_ui/src/conversation_view/thread_search_bar.rs` +962 lines, `agent_ui/src/conversation_view/thread_view.rs` +1094 lines, `extensions_ui.rs` +286 lines, `feature_flags/src/flags.rs` +16 lines, `title_bar/src/title_bar.rs` +36 lines, `agent/src/sandboxing.rs` +458 lines, `agent/src/tools/terminal_tool.rs` +957 lines) — yet `git merge` produced only one trivial conflict (above).

- **Critical Fix #1** (`load_session` via `pending_sessions` shared-task pattern): intact at `agent/src/agent.rs:399/572/1612`.
- **Critical Fix #3** (`content_only()`): intact at `acp_thread.rs:335` (was 262 pre-merge; upstream shifted it downward).
- **Critical Fix #6/#9** (`stopped_emitted_for_task`): intact at `acp_thread.rs:2887/2931/3026`.
- **Critical Fix #8** (`drop(turn.send_task)`): intact at `acp_thread.rs:3079`.
- **Critical Fix #11** (entity-identity guard): intact via `ThreadMetadataStore` session_id lookup at top of `load_agent_thread` in `agent_panel.rs`.
- **PR #50** `session_creation_chain` + `_settings_subscription` coexistence: intact at `agent_servers/src/acp.rs:438-439`.
- **PR #55** streaming-reveal `EntryUpdated` emit: intact (16 occurrences in `acp_thread.rs`).
- **PR #56 Fix 1a** deferred `UserCreatedThread`: intact in `external_websocket_sync/src/thread_service.rs`.
- **PR #56 Fix 1b** cfg-gated early return: intact and verified as the FIRST statement of `BaseView::Uninitialized` at `agent_panel.rs:5468-5473` (was 5420 pre-merge; upstream added +49 lines above).
- **PR #60** `ede_diagnostic` retry loop: intact at `thread_service.rs:1916/1976`.
- **PR #63** claude-agent-acp wedge recovery surface (`force_reset_session`, `clear_keep_alive`, `track agent_name per thread`, dispatch diagnostics stripped): intact in `thread_service.rs` (Helix-only, no upstream churn).
- **PR #64** `agent_ready` re-emit on reopening already-loaded thread: intact (Helix-only, no upstream churn).
- **AcpBetaFeatureFlag override**: intact in `feature_flags/src/flags.rs:30` (despite +16 line upstream churn around it).
- **Three `// HELIX: External agent …` bypass markers** in `extensions_ui.rs`: intact at lines 337, 359, 1629 (shifted from 226/248/1518 pre-merge — upstream added new chips above them).
- **`render_restricted_mode` cfg-gated early return** in `title_bar.rs`: intact at line 699 (was 678 pre-merge).
- **`build_application(headless: bool)`** in `zed/src/main.rs`: intact at line 88; `--headless` arg threading at line 346.

### Risks Considered in Round 2 Reconnaissance — Outcomes

- **`agent_ui/src/conversation_view.rs` +1024 lines + new `thread_search_bar.rs` (+962 lines) + `thread_view.rs` +1094 lines** — the "in-thread search bar" cluster (`10628c3d2c` agent_ui: Add in-thread search bar (#57231)) was the predicted signature-drift magnet for `from_existing_thread()`. **Outcome**: auto-merged cleanly — field set on `ConversationView::new` did not change in a Helix-incompatible way. Build green confirms `from_existing_thread()` still type-checks.
- **`acp_thread/src/acp_thread.rs` +198 lines** — `d8eb569cc5` "acp_thread: Fix compaction button stuck at loading state on error (#59161)" risked clashing with PR #55's emit site and Critical Fix #6's invariant. **Outcome**: auto-merged. `EntryUpdated` site survives; `stopped_emitted_for_task` guard sites all intact; E2E Phase 15 (PR #55) and Phase 9 (Stopped invariant) green.
- **`agent/src/agent.rs` +223 lines + `agent/src/thread.rs` +511 lines** — `33a54ce423` "Support provider-side compaction in the language model clients (#59145)" + `ea035a7a46` "Account for max_output_tokens in compaction threshold (#59469)" + `8ad9d18b93` "Address review follow-ups for quit-time thread flush (#59079)" + `f16a46967b` "Don't pass back the diff on successful edits (#59335)". **Outcome**: auto-merged. Critical Fix #1's `pending_sessions` shared-task path intact; build green; E2E green.
- **Sandboxing cluster** (`5ef7b14a7d` enable sandboxing for staff by default, `dd7e50e66b` add global setting, `dfd44a45dd` Windows WSL, `54cd092189` sandbox permissions settings page, `e52cc1abe9` better sandboxing UI, `6661273a41` linux sandboxing, `8117571de3` host allowlist, `bf746a7a26` NetworkAccess enum, `c8faadd9c8` allowlisting proxy server, `c52d06a8f3` upstream proxy configuration, `f73b9daf3b` hostname allowlist, `3c7ca5ec5d` in-process proxy, `2c0a044237` unrestricted network access explicit, `a3e15cefac` show command in permission prompts, `ce22d1ab28` show commands in prompts) — 458 lines added to `agent/src/sandboxing.rs`, 957 added to `agent/src/tools/terminal_tool.rs`. **Outcome**: auto-merged. No `external_websocket_sync` interaction; Helix mode does not interact with the new sandbox UI (which is opt-in via settings).
- **`agent_panel.rs` +203 lines** — `e45e42af6e` "Use the thread title for agent notifications (#59377)" + `50b2f63cfe` "Fix click flicker on empty draft thread (#59342)" + `9622ae92e1` "Fix reporting of 401/403 errors (#59119)" + `02b62a3d1f` "Settings UI agent providers (#58876)" + `ea673ff233` "Fix copy shortcut in macOS notebook cells" risked Fix 1b position regression. **Outcome**: auto-merged. Fix 1b is still the FIRST statement of `BaseView::Uninitialized` (shifted from line 5420 → 5468 — content of comment block + `#[cfg(...)] return` block all preserved verbatim).
- **`extensions_ui.rs` +286 lines** — `5e32405669` "Add `RebuildDevExtension` action (#55173)" + further chip-area refactors. **Outcome**: three `// HELIX: External agent …` markers intact at new lines 337, 359, 1629 (shifted from 226, 248, 1518 — file got new content above them).
- **`feature_flags/src/flags.rs` +16 lines** — new feature flags added. **Outcome**: `AcpBetaFeatureFlag::enabled_for_all() -> true` override intact at line 30.
- **`title_bar/src/title_bar.rs` +36 lines** — restructure around title bar. **Outcome**: Helix `Plan` import + cfg-gated `external_websocket_sync::{...}` imports + `render_restricted_mode` cfg-gated early return all intact.
- **`zed/src/main.rs` +7 lines** + `zed/src/zed/open_listener.rs` +163 lines (from `adb8a1d80f` Respect `default_open_behavior` when opening from Finder). **Outcome**: `--allow-multiple-instances`, `--headless`, `build_application(headless: bool)` pattern all intact.
- **`settings_content/src/settings_content.rs`** — `2c0a044237` and the sandbox cluster added several fields to top-level Settings. **Outcome**: Helix's `helix_mode`, `suggest_dev_container`, `auto_open_panel`/`show_onboarding`, `dev_container_use_buildkit` (added in round 1) all coexist cleanly.

### Cargo.toml / Cargo.lock notes

- `acp_thread/Cargo.toml`: +1 line (new dep for compaction UI). Auto-merged.
- `agent/Cargo.toml`: +2 lines (sandbox deps). Auto-merged.
- `agent_ui/Cargo.toml`: +1 line. Auto-merged.
- `extensions_ui/Cargo.toml`: +2 lines. Auto-merged.
- `zed/Cargo.toml`: +3 lines. Auto-merged.
- `Cargo.toml` workspace: new deps + version bumps. Auto-merged.
- `Cargo.lock`: auto-merged; rebuilt cleanly.

### Pre-existing Breakage Repaired

**None this round.** Build green on first try (3m 37s warm cache). Two unused-import warnings present, both incidental:
- `crates/agent_ui/...` (upstream code)
- `crates/zed/src/zed.rs:871` — `use settings::Settings;` (Helix-introduced wrapper at `external_websocket_sync` init site; the underlying `ExternalSyncSettings::get_global(cx)` no longer needs the trait import since upstream re-exported the relevant method on a concrete type). Innocuous; can be removed in a follow-up cleanup pass.

This is the **third consecutive** merge with zero "Pre-existing Breakage Repaired" — 002077 → 002100 round 1 → 002100 round 2.

### Validation

- `./stack build-zed dev`: **PASSED** (3m 37s warm cache, 0 errors, 2 unused-import warnings).
- Silent-drift sweep: **all clean** — every critical-fix grep returned the expected hit; Helix bypass markers all present at their new (shifted) line numbers.
- E2E `zed-agent` only: **PASSED on first try** — all 15 phases green, store validation PASSED, 14 interactions / 0 interrupted/cancelled / response-entries isolation PASSED / thread-title sync PASSED.
- E2E `zed-agent,claude`: **PASSED** on retry with full rebuild. First attempt was via `--no-build` (cached test server) and the store-validation step reported a `response_entries leaked across interactions` isolation violation — re-running with full rebuild produced green across the board (28 interactions / 0 interrupted-cancelled / response-entries isolation across 8 sessions / thread-title sync across 3 sessions). This matches the documented e2e-test CLAUDE.md guidance: "Never trust `--no-build` when investigating test failures."

### Lesson learned (added to future-merge rebase checklist)

- **`--no-build` is a footgun for the E2E test server.** When the Helix Go code (`api/`) has advanced relative to the cached test-server binary, the `--no-build` shortcut runs an out-of-date server against current Zed and can produce false positive store-validation failures (`response_entries leaked across interactions` was the symptom this round). Always do a full `./run_docker_e2e.sh` rebuild when investigating a failure. The cached binary is only safe when the Go code is provably unchanged since the last rebuild.



**Divergence at start**:
- Fork HEAD: `f82e1c6760` (Merge PR #61 — 002077, landed 2026-06-12)
- Upstream HEAD: `a31d3505da` ("git: Do not run `git stash list` on every file save (#59042)")
- Upstream commits to merge: **25** (3 days of activity since 002077's `992f395c3d`)
- Helix-only commits since 002077: **0** (fork main has not moved)

Smallest catch-up window of any merge in this series.

### Conflicts and Resolutions

#### 1. `crates/settings_content/src/settings_content.rs` — both sides added a field
**Upstream change**: `26fc42721a` "dev_container: Support the classic Docker builder via a setting (#59288)" added `pub dev_container_use_buildkit: Option<bool>` to `RemoteSettingsContent`.
**HEAD change**: Helix's earlier `suggest_dev_container: Option<bool>` field on the same struct (drives the `dev_container_suggest.rs` early-return guard).
**Resolution**: keep both fields. Standard "both sides added a field" merge — independent settings, no semantic overlap.
**Risk**: none. Both fields are `Option<bool>` with defaulting handled at consumer sites.

### Helix Surface — Auto-Merge Survival Check

All Helix critical fixes and load-bearing patches survived `git merge upstream/main` cleanly. No upstream commits in this window touch `acp_thread/`, `agent/src/`, `workspace.rs`, `zed/src/main.rs`, `title_bar/`, `feature_flags/`, `agent_servers/`, `external_websocket_sync/`, or `agent_settings/`, so the "must survive" surface is by-construction untouched.

- **Critical Fix #1** (`load_session` via `pending_sessions` shared-task pattern): intact in `agent/src/agent.rs:399/572/1612/1627/1637`.
- **Critical Fix #3** (`content_only()`): intact at `acp_thread.rs:262`.
- **Critical Fix #6/#9** (`stopped_emitted_for_task`): intact at `acp_thread.rs:2793/2837/2929`.
- **Critical Fix #8** (`drop(turn.send_task)`): intact at `acp_thread.rs:2980`.
- **Critical Fix #11** (entity-identity guard via `ThreadMetadataStore` / `external_websocket_sync::get_thread`): intact in `agent_panel.rs:4623+`.
- **PR #50** `session_creation_chain` + `_settings_subscription` coexistence: intact at `agent_servers/src/acp.rs:438-439`.
- **PR #55** streaming-reveal `EntryUpdated` emit: intact (16 occurrences in `acp_thread.rs`, no upstream churn).
- **PR #56 Fix 1a** deferred `UserCreatedThread`: intact in `external_websocket_sync/src/thread_service.rs` (zero churn either way).
- **PR #56 Fix 1b** cfg-gated `return;`: intact as the FIRST statement of `BaseView::Uninitialized` at `agent_panel.rs:5420-5425`. `1e017d04b9`'s `Rules Library` menu deletion landed in a different region of the same file (line ~5690) with no positional impact.
- **PR #60** `ede_diagnostic` retry loop: intact at `thread_service.rs:1734/1761` (zero churn either way).

### Risks Specifically Worried About in Planning — All Cleanly Absorbed

- **`f39cf25c0b` "extension_ui: Hide agent servers from chips (#59231)"** — restructured the `ExtensionProvides::iter()` chip filter from `.filter_map` to `.filter().map()` at upstream line ~1738. Auto-merged with no manual conflict; the three `// HELIX: External agent …` bypass markers remain at lines 226, 248, 1518 (no line-number shift — the upstream restructure was confined to its own region).
- **`1e017d04b9` "agent_ui: Remove dead link in agent menu (#59232)"** — single-hunk deletion of the `Rules Library` menu entry near line 5690. Auto-merged with no manual conflict; Fix 1b's first-statement position in `BaseView::Uninitialized` at line 5420 is untouched.
- **`d4cc8d2409` "Patch async-process to allow reusing their reaper (#59156)"** — added `[patch.crates-io] async-process = …` entry in `Cargo.toml`. Helix had no prior `[patch.crates-io]` entry, so this landed as a clean upstream-only addition.
- **`26fc42721a` "dev_container: Support the classic Docker builder via a setting (#59288)"** — the lone conflict (above). Trivial both-sides-added-a-field on `RemoteSettingsContent`.

### Cargo.toml / Cargo.lock notes

- `objc2-app-kit`: `0.3` → `0.3.2` with feature widening (`NSButton`, `NSControl`, `NSResponder`, `NSView`, `NSWindow`, `objc2-core-foundation`). `objc2 = "0.6"` added as a workspace dep.
- New `[patch.crates-io] async-process = { git = "https://github.com/zed-industries/async-process.git", rev = "0b6d6713570af61806e1e5cb40e0f757cb93fd9d" }` (from `d4cc8d2409`).
- `Cargo.lock` auto-merged.
- Helix workspace members (`crates/cloud_api_types`, `crates/external_websocket_sync`) and the `rust-embed`'s `debug-embed` feature are intact and untouched.

### Pre-existing Breakage Repaired

**None this round.** Build green on first try; no Helix-side signature-drift repairs needed. Second consecutive merge with zero "Pre-existing Breakage Repaired" entries (after 002077).

### Validation

- `./stack build-zed dev`: **PASSED** — cargo 16m 59s, total ~18m (cold cache), 0 errors, 1 unused-import warning in upstream code. Binary 220M at `helix/zed-build/zed`.
- Silent-drift sweep: **all clean** — `smol::Timer` 0 hits, `ActiveView`/`set_active_view`/`draft_threads`/`background_threads`/`selected_agent_type` 0 hits, Fix 1b first-statement intact, three `// HELIX:` markers intact, all PR #50/#55/#56/#60 surface intact.
- E2E `zed-agent` only: **PASSED** on retry (first run timed out at Phase 9 with ~73s API latency before first token + 17s of streaming = 90s budget exhausted; retry green with Phase 9 explicitly reporting "Received 2 completions — thread recovered from rapid cancel (correct)"). Lesson: Phase 9's 90s budget is tight when zed-agent's Anthropic API has a slow first-token latency for long-form prompts ("Write a detailed explanation of merge sort with code examples."). The flake is API-latency-bound, not a Helix regression.
- E2E `zed-agent,claude`: **PASSED** on retry (first run: zed-agent ALL 15 phases green; claude Phase 1 timeout with 0 events — npx-install bootstrap flake, also documented from 001996). Second run: `[zed-agent] PASSED`, `[claude] PASSED`, `[store] PASSED`. 28 interactions, 0 interrupted/cancelled, response entries isolation across 8 sessions, thread title sync across 3 sessions. Phase 9 (PR #60 retry-loop gate), Phase 15 (PR #55 emit gate, 82 samples / 407ms longest gap / 22% in final 20%), Phase 16 (0 spontaneous user_created_thread emits — Fix 1a working) all explicitly green.



**Divergence at start**:
- Fork HEAD: `ecdc2ea67d` (PR #60 — `claude-agent-acp` `ede_diagnostic` retry loop, landed 2026-06-09)
- Upstream HEAD: `992f395c3d` ("editor: Fix columnar selection alignment on rows with multi-byte chars (#57097)")
- Upstream commits to merge: **256** (10 days of activity since 002029-extension-round-2's `9d50bab893`)
- Helix-only commits since 002029: 2 (PR #60: `27e8867c9e` ede_diagnostic retry, `e4c36d837c` cleanup)

### Conflicts and Resolutions

#### 1. `.github/workflows/{run_cron_unit_evals,run_unit_evals}.yml` — modify/delete
**Upstream change**: deleted both files (Zed CI workflows retired).
**HEAD change**: had small modifications.
**Resolution**: `git rm` — Helix doesn't use Zed's CI.
**Risk**: none.

#### 2. `.github/workflows/slack_notify_first_responders.yml` — content
**Resolution**: `git checkout --theirs`. Helix doesn't use Zed's CI.
**Risk**: none.

#### 3. `crates/language_model/src/model/mod.rs` — rename/delete
**Upstream change**: PR `a98485809b` "Return typed completion errors from Cloud provider (#58997)" plus the `language_model_core` refactor — `crates/language_model/src/model.rs` (HEAD's `model/mod.rs`) deleted entirely; `CloudModel` code relocated to `crates/language_model_core/` and `crates/language_models/src/provider/cloud.rs`.
**HEAD change**: had `model/mod.rs` with `pub mod cloud_model; pub use cloud_model::*;` — no Helix content.
**Resolution**: accept upstream deletion (`git rm`). No Helix surface referenced `cloud_model` directly.
**Risk**: none. Verified `git grep "cloud_model\|CloudModel" -- crates/external_websocket_sync/ crates/agent_ui/ crates/agent/` is clean.

#### 4. `crates/recent_projects/src/dev_container_suggest.rs` — content
**Upstream change**: added `use std::path::Path;` (new use of `Path` in `worktree_abs_path: &Path` parameter).
**HEAD change**: had `use settings::Settings;` (Helix-only — required for `RemoteSettings::get_global(cx).suggest_dev_container` early-return guard).
**Resolution**: keep both imports.
**Risk**: none — both compile and are needed.

#### 5. `crates/title_bar/src/title_bar.rs` — content
**Upstream change**: added `use command_palette_hooks::CommandPaletteFilter;` import (new use in `CommandPaletteFilter::update_global(cx, ...)` call site).
**HEAD change**: had Helix-only `use cloud_api_types::Plan;` and the cfg-gated `use external_websocket_sync::{WebSocketConnectionStatus, get_websocket_connection_status};` block.
**Resolution**: keep all three import lines. Reordered to alphabetical-ish layout to match the surrounding import block style.
**Risk**: none.

### Helix Surface — Auto-Merge Survival Check

All Helix critical fixes and load-bearing patches survived `git merge upstream/main` cleanly with no manual conflict in the Helix-touched source files (`agent.rs`, `acp_thread.rs`, `agent_panel.rs`, `conversation_view.rs`, `workspace.rs`, `agent_servers/src/acp.rs`, `zed/src/main.rs`, `extensions_ui.rs`, `feature_flags/src/flags.rs`):

- **Critical Fix #1** (`load_session` keeps `Entity<NativeAgent>` alive): preserved via the `pending_sessions` shared-task pattern in `open_thread` (pre-existing relocation from 002029-era).
- **Critical Fix #3** (`content_only()`): intact at `acp_thread.rs:262`.
- **Critical Fix #6/#9** (`stopped_emitted_for_task`): intact at `acp_thread.rs:2793/2837/2929`.
- **Critical Fix #8** (`drop(turn.send_task)`): intact at `acp_thread.rs:2980`.
- **Critical Fix #11** (entity-identity guard via `ThreadMetadataStore` session_id lookup): intact at top of `load_agent_thread` in `agent_panel.rs`.
- **PR #50** `session_creation_chain` + `_settings_subscription` coexistence: intact in `agent_servers/src/acp.rs`.
- **PR #55** streaming-reveal `EntryUpdated` emit: intact at `acp_thread.rs:2147`.
- **PR #56 Fix 1a** deferred `UserCreatedThread`: intact in `external_websocket_sync/src/thread_service.rs`.
- **PR #56 Fix 1b** cfg-gated early return: intact and verified as the FIRST statement of `BaseView::Uninitialized` in `ensure_thread_initialized` at `agent_panel.rs:5420`.
- **PR #60** `ede_diagnostic` retry loop: intact in `handle_follow_up_message` (no upstream churn in `thread_service.rs` this window).

### Risks Specifically Worried About in Planning — All Cleanly Absorbed

- **`d7ac5e6cf4` "Preserve waiting tool call status on updates (#58537)"** — +602 lines reworking `ToolCall::status`. Auto-merged with no manual conflict; PR #55's `EntryUpdated` emit site and Fix #6's `stopped_emitted_for_task` invariant both survived in place. Will validate at runtime via E2E Phases 15 (PR #55) and 8/9 (Stopped invariant).
- **Compaction cluster** (`e5052961af`, `9baefe701e`, `e17e272d24`, `5c90b0664f`, `0bc6c76fcf`, `0e9e8d0e68`, `620ceaaaca`) — ~1700 net lines. Auto-merged with no manual conflict. No `compact`/`Compact`/`compaction`/`cumulative_token_usage` matches in `external_websocket_sync/` so WS payload schema is unchanged. The `auto_compact` settings field coexists with Helix's `show_onboarding` / `auto_open_panel` / `sandbox_permissions` without conflict.
- **`215ca2fb0b` "Typed workspace errors (#57649)"** + follow-up `83aa943705` — auto-merged with no manual conflict. Grep across `crates/external_websocket_sync/` and `crates/agent_ui/src/` shows **zero Helix `Workspace::show_error` call sites** — the typed-error migration is not needed in the Helix surface. (Helix relies on log/`anyhow` propagation, not on `Workspace::show_error`.) No "Pre-existing Breakage Repaired" entry needed.
- **`116e4bc184` "Inherit source agent without draft content"** — auto-merged with no manual conflict. Fix 1b remains the FIRST statement of `BaseView::Uninitialized`; the source-agent inheritance path falls inside the non-`external_websocket_sync` branch and is therefore suppressed in Helix builds.
- **`27191913e9` "Cumulative token usage" + `0bc6c76fcf` "Hide token usage after /compact"** — auto-merged. No `cumulative_token_usage`/`TokenUsage` matches in `external_websocket_sync/`; WS payload schema unaffected.
- **`620ceaaaca` "Flush thread content to database on app quit"** — auto-merged. The flush path is in `NativeAgent` (database-backed); Helix's WS sync layer is orthogonal. No interaction.
- **`56b71271c4` "Enable ACP session usage and deletion features"** — auto-merged. Helix `NativeAgentConnection` impl already supports `close_session` (preserved upstream); no new override needed.

### Pre-existing Breakage Repaired

**None this round.** Build (`./stack build-zed dev`) succeeded on first try after the merge, with only three unused-import warnings (all in upstream code: `crates/title_bar`, `crates/agent_ui`, `crates/zed`). No Helix-side signature-drift repairs were needed — first time since 001980.

### Validation

- `./stack build-zed dev` (8m 14s, 0 errors, 3 unused-import warnings in upstream code): **PASSED**
- Silent-drift sweep (`ActiveView`/`set_active_view`/`draft_threads`/`background_threads`/`selected_agent_type`/`smol::Timer`/non-tuple `Stopped`/`Workspace::show_error`/`cumulative_token_usage`/`compact`-in-WS): **all 0 hits or only expected matches**
- E2E (zed-agent + claude personalities): **see commit history below for run results**

## Merge 001996 (2026-05-11)

**Divergence at start**:
- Fork HEAD: `fe8f4f4e3f` (PR #53 — sidebar split-brain Critical Fix #11)
- Upstream HEAD: `8bdd78e023` ("opencode: Update Free models (#56328)")
- Upstream commits to merge: **127** (3 days of activity since 001980's `1da60a8518`)
- Helix-only commits since 001980: 3 (PRs #51 `--headless`, #52 `cancel_current_turn`, #53 sidebar split-brain)

### Conflicts and Resolutions

(Updated incrementally as each conflict is resolved.)

#### 1. `crates/acp_thread/src/acp_thread.rs` — `run_turn()` cancel/Stopped path
**Upstream change**: PR #55562 (`0a52f80824` "acp_thread: Clear `running_turn` when prompt task drops tx") reordered the `run_turn` post-spawn block so `running_turn.take()` runs **before** the `let Ok(response) = response else { return Ok(None); }` early return. Without this, dropping the inner `send_task` left `running_turn` populated and the panel stuck in `Generating`.
**HEAD change**: Helix had a separate (earlier) early return for the dropped-tx case that did its own same-turn cleanup AND emitted `Stopped(Cancelled)` guarded by `stopped_emitted_for_task` (Critical Fixes #6, #8, #9 — exactly one Stopped per send, even on rapid cancel/interrupt).
**Resolution**: kept upstream's reorder (single same-turn `running_turn.take()` block before the early return) and folded Helix's `Stopped(Cancelled)` emission with `stopped_emitted_for_task` guard into the dropped-tx branch. The Helix duplicate-guard for the natural-completion `Stopped` emission a few hundred lines below was untouched (no conflict there).
**Risk**: medium. This is the highest-traffic code path in the merge (Phase 8/9/13/14 of E2E). The combined logic is functionally a strict superset of both sides; the Helix-only invariant "exactly one `Stopped` per `send()`" is preserved. Validation: E2E phases 8 (mid-stream interrupt), 9 (rapid 3-turn cancel), 13 (`cancel_current_turn` happy path), and 14 (`cancel_current_turn` no-op) all stress this path.
**Reasoning trail**: see commit `bf544922aa` and the `stopped_emitted_for_task` documentation in the code.

### Pre-existing Breakage Repaired

#### `crates/agent_ui/src/agent_panel.rs` — `BaseView` non-exhaustive match (Helix UI state query)
**Upstream change**: added a third `BaseView::Terminal { terminal_id }` variant to support the new agent-panel-as-terminal mode (PR #56233 area).
**HEAD change**: the Helix UI state query loop (cfg-gated, callback handler set up in `AgentPanel::new()`, around `agent_panel.rs:1270`) only matched `BaseView::AgentThread { conversation_view }` and `BaseView::Uninitialized`.
**Resolution**: added a `BaseView::Terminal { .. } => ("terminal".to_string(), None, 0, None)` arm. Reports the active view as `"terminal"` with no thread/entry/model context (matches the headless responder's pattern of just reporting the active surface name).
**Risk**: low. The `terminal` active_view value is new in the protocol — Helix server may need to teach itself that this is a known value. For now the test only asserts on `agent_thread`/`agent_thread_loading`/`uninitialized`, so this is forward-compatible.
**Lesson for future merges**: when upstream adds a variant to a Helix-touched enum, the silent-drift sweep won't catch it (sweep is for renames/removals). Build-driven discovery is the only safety net here. Add `BaseView` to the post-merge enum-arm review list.

#### `crates/external_websocket_sync/src/thread_service.rs` — Phase 13 race
**Discovered by**: E2E Phase 13 failing on the first run with this merge — `message_completed` arrived BEFORE `turn_cancelled` on the wire, so `handleTurnCancelled` saw `interaction.State == Completed` (not `Waiting`) and never transitioned to `Interrupted`.
**Root cause**: GPUI flushes queued events at the END of an entity update closure, BEFORE the outer `cx.update` returns. The cancellation handler used to call `thread.cancel(cx)` (which emits `Stopped(Cancelled)` synchronously) and only THEN send `turn_cancelled`. The persistent `AcpThreadEvent::Stopped(_)` subscription's `MessageCompleted` send always won the race.
**Resolution**: probe `thread.status()` first; if `Generating`, send `turn_cancelled{status:cancelled}` BEFORE invoking `cancel()`; if not running, send `turn_cancelled{status:noop}`. The cancel itself still fires Stopped → MessageCompleted, but Helix has already marked the interaction Interrupted by then, and the State refresh in `handleMessageCompleted`'s flush path (`websocket_external_agent_sync.go:1765-1772`) prevents it from clobbering.
**Risk**: low — the new ordering is strictly better; the noop branch is also more accurate now (was previously sending `cancelled` for a thread that exists but has no running turn).
**Note**: this race likely existed since PR #52 added Phase 13/14 (Apr 2026). Whether the test ever passed is unclear; may have been masked by faster LLM responses or different upstream timing.



**Divergence at start**:
- Fork HEAD: `f5fab97857` (PR #47)
- Upstream HEAD: `1da60a8518` ("editor: Extract Diagnostics code out of `editor.rs` (#55747)")
- Upstream commits to merge: **172** (10 days of activity since 001909's `e3d1876c06`)
- Fork commits ahead of upstream: 203 (entire Helix surface)

Two intermediate plans (001946, 001947 — both 2026-04-27) were **never executed**. As a result this merge spans 10 days of upstream activity rather than 2.

### Conflicts and Resolutions

(Updated incrementally as each conflict is resolved.)

#### 1. `.github/workflows/deploy_cloudflare.yml` — modify/delete
**Upstream change**: deleted the file (Cloudflare deploy workflow retired upstream).
**HEAD change**: had small unrelated modifications.
**Resolution**: `git rm` — accept upstream deletion. Helix doesn't use Zed's CI.
**Risk**: none.

#### 2. `Cargo.lock` — content
**Resolution**: `git checkout --theirs` (always — regenerated on next build with Helix features).
**Risk**: none.

#### 3. `crates/agent_settings/src/agent_settings.rs` — content
**Upstream change**: PR #55575 ("Remove new thread location setting") removed the `NewThreadLocation` import, the `new_thread_location` field on `AgentSettings`, and its initialiser.
**HEAD change**: Helix-only fields `show_onboarding` and `auto_open_panel` were added in the same struct/initialiser blocks alongside `new_thread_location`.
**Resolution**: kept Helix's `show_onboarding` and `auto_open_panel`; dropped `new_thread_location` to match upstream removal. Also dropped the now-orphaned `NewThreadLocation` import. The `NewThreadLocation` type no longer exists anywhere in the workspace.
**Risk**: none. Verified `grep -rn "new_thread_location\|NewThreadLocation" crates/` is clean.

#### 4. `crates/gpui_wgpu/src/wgpu_renderer.rs` — content
**Upstream change**: comment-only addition (`// TBD. Does retrying more actually help?`) inside a GPU error retry block, plus larger non-conflicting work for BGR subpixel layout and `WgpuContext::new_rejecting_software`.
**HEAD change**: none in the conflicting region; only the absence of the new comment.
**Resolution**: accept upstream — keep the comment.
**Risk**: none. Helix doesn't touch the wgpu renderer.

### Pre-existing Breakage Repaired

#### `crates/acp_thread/src/acp_thread.rs` — `matches!(event, AcpThreadEvent::Stopped)` (line 5357 + 5429)
Two test-only call sites in the Helix-added `test_second_send_during_active_turn_emits_stopped_for_both_turns` (Critical Fix #6 verification) and `test_dropped_send_task_clears_running_turn` were using the unit-variant pattern `AcpThreadEvent::Stopped` after `Stopped` became a tuple variant `Stopped(StopReason)`. Updated to `AcpThreadEvent::Stopped(_)`. This was likely broken since the 001864 merge (when `StopReason` was added) but never noticed because production builds don't compile `#[cfg(test)]`.

**Lesson for future merges**: when porting-guide checklist item 12a says "Pattern matches must use `Stopped(_)`", it applies to test code as well. Add a grep to the silent-drift sweep:

```bash
grep -n "AcpThreadEvent::Stopped\b\([^(]\|$\)" crates/acp_thread/src/acp_thread.rs
```

(Pattern: any `AcpThreadEvent::Stopped` not followed by `(`.)

## Merge 002029-extension round 2 (2026-06-02)

A third upstream merge stacked onto the 002029 feature branch (still open PR). 242 upstream commits absorbed; four manual conflicts (all trivial "both sides added a struct field/var" merges); three signature-drift repairs.

**Divergence at start (of round 2 extension)**:
- Branch HEAD: `fb97e2cf95` (002029-extension first round)
- Upstream HEAD: `9d50bab893` ("git_ui: Add total diff stats to git panel (#58018)")
- Upstream commits to merge: **242** (8 days since `13e7c11768`)

**Manual conflicts** (merge commit `1ebfaf5a39`):

- `crates/zed/src/main.rs`: upstream extracted `build_application()` as a helper that always passes `false` for the platform-headless flag. Helix needs `args.headless` propagated so the `--headless` CLI flag selects the headless gpui_platform. **Resolution**: take `headless: bool` as a parameter on `build_application()`, and at both call sites pass either `args.headless` (the main entry) or `false` (the early-error path in `files_not_created_on_launch`).
- `crates/agent_servers/src/acp.rs` (3 hunks): each is a "both sides added a struct field" merge — Helix's `session_creation_chain` field/initializer (PR #50) coexists with upstream's new `_settings_subscription`. Keep both, both initializers.
- `crates/agent_settings/src/agent_settings.rs` and `crates/settings_content/src/agent.rs`: Helix's `show_onboarding` / `auto_open_panel` fields coexist with upstream's new `sandbox_permissions` field. Keep all.

**Pre-existing Breakage Repaired** — three signature-drift repairs in commit `dcd8622f99`:

- `agent_servers::AgentServerDelegate::new` now takes a third arg, `loading_status_tx: Option<watch::Sender<Option<String>>>`. Three Helix call sites in `external_websocket_sync/src/thread_service.rs` (lines ~1438, ~1795, ~2013) all pass `None` — these are internal session-creation paths with no UI status display.
- `agent_ui/src/conversation_view.rs::from_existing_thread` (Helix-only constructor): upstream removed `prompt_store` entirely from `ConversationView`, `AgentPanel`, `EntryViewState::new`, `ModelSelectorPopover::new`, and `ThreadView::new` (now 24 args instead of 25). Mirror upstream by dropping `prompt_store` from `from_existing_thread`'s signature, dropping it from the three constructor calls, dropping the `prompt_store,` field from the trailing `Self { ... }`. Also add `loading_status: None` to the `Self` block (upstream `new()` now has it; we missed it earlier).
- `agent_ui/src/agent_panel.rs` (2 callers of `from_existing_thread`): drop `this.prompt_store.clone()` / `let prompt_store = self.prompt_store.clone();` — these were the only readers of `AgentPanel::prompt_store` (already removed by upstream).

**Ancillary upstream notes (no Helix action required)**:

- `c413552859` "Update agent-client-protocol sdk to 0.13.1" — internal acp wire-protocol bump; the new `additional_directories` capability we already plumb through `SessionDirectories` from round 1 is unchanged.
- `4d32f41ef6` "Remove audio denoiser crate" — workspace member removed cleanly; nothing in the Helix surface referenced it.
- `201ae99dce` "Implement compaction (experimental)" — adds compaction code in upstream-only paths; cfg-gated under experimental flags, doesn't touch Helix surface.

**Validation**:
- `./stack build-zed dev`: green (after the three repairs).
- E2E `zed-agent`: **PASSED** (all 17 phases).
- E2E `claude`: **PASSED** (all 17 phases, including Phase 17 live-Claude-process-count gate that proves PR #56 Fix 1b draft-suppression survived).
- Store validation: PASSED (28 interactions, 0 interrupted/cancelled).

## Merge 002029-extension (2026-05-25)

A second upstream merge stacked onto the 002029 feature branch before that PR landed (the original 002029 PR was still open; reviewer asked to roll a fresh upstream into the same branch rather than spin up a new task).

**Divergence at start (of extension)**:
- Branch HEAD: `8692f073b2` (002029 first-round merge, post `e60a1b2789` re-merge)
- Upstream HEAD: `13e7c11768` ("ep: Fix bugs in the `split-commit` command (#57604)")
- Upstream commits to merge: **287** (3 days since 002029's `1399540715`)

**Merge result**: `git merge upstream/main` resolved entirely via the `ort` strategy — **no manual conflicts**. The Helix surface in `agent_panel.rs`, `conversation_view.rs`, `agent_servers/acp.rs`, `connection.rs`, `agent.rs`, `workspace.rs` all auto-merged cleanly. Critical Fix #11, Fix 1b, PR #50 `session_creation_chain`, the title_bar restricted-mode override, and the extensions_ui Helix bypass markers all survived intact (verified by grep).

**Pre-existing Breakage Repaired** — one signature drift, applied in `f226fe7604`:

- `crates/agent_ui/src/conversation_view.rs::from_existing_thread`: upstream `cfd0461b5a` ("Prefix `read_file` tool output with line numbers") added a `code_span_resolver: AgentCodeSpanResolver` field to `ConversationView` and a new positional argument to `ThreadView::new` (now 25 args). Mirror what upstream's `new()` does at line 725: build the resolver via `AgentCodeSpanResolver::new(&project.downgrade(), cx)`, pass it as a `.clone()` between `project.downgrade()` and `thread_store.clone()` in the `ThreadView::new` call, and add `code_span_resolver` to the trailing `Self { ... }`. The upstream `new()` also wires a `project::Event::Worktree*` subscription that calls `resolver.clear_cache()` — Helix's `from_existing_thread` does not currently bind a `Conversation`-level project subscription, so we don't add one (the resolver cache will simply persist for the lifetime of the headless wrapper; acceptable for the WebSocket-sync path where worktrees are not user-mutated mid-session).

**Ancillary upstream notes (no Helix action required)**:

- `91531fad6d` "ACP logout" — adds `supports_logout`/`logout` defaults to the `AgentConnection` trait. Helix's UI-state query loops in `agent_panel.rs` and `zed/main.rs` don't enumerate logout, so no exhaustiveness break.
- `dee596fa96` "ACP additional directories" — extends the `additional_directories` capability already wired through `SessionDirectories`. Composes with PR #50 with no chain-wrapper changes needed.
- `6753eb1736` "Update skill settings immediately after changes" — touches `agent.rs` but only inside upstream-only paths; no Helix surface affected.
- `cfd0461b5a`, `f78f6da255` — `conversation_view.rs` and `thread_view.rs` field additions; the `code_span_resolver` repair above covers both.

**Validation**:
- `./stack build-zed dev`: green (one signature-drift fix).
- E2E `zed-agent`: **PASSED** (all 17 phases).
- E2E `claude`: **PASSED** (all 17 phases, including Phase 17 live-Claude-process-count gate).
- Store validation: PASSED (28 interactions, 0 interrupted/cancelled).

## Merge 002029 (2026-05-21)

**Divergence at start**:
- Fork HEAD: `fd26c1a113` (Dockerfile.ci helix-org fix)
- Upstream HEAD: `1399540715` ("settings_ui: Display scope in the breadcrumb (#57437)")
- Upstream commits to merge: **261** (10 days of activity since 001996's `8bdd78e023`)
- Helix-only commits since 001996: 5 (PRs #50 `session_creation_chain`, #55 streaming-reveal `EntryUpdated`, #56 deferred-emit + Fix 1b draft suppression, #57 Phase 16 counter fix, direct `fd26c1a113` Dockerfile.ci)

### Conflicts and Resolutions

(Updated incrementally as each conflict is resolved.)

#### 1. `.github/workflows/compare_perf.yml` and `release_nightly.yml`
**Resolution**: `git checkout --theirs` for both — Helix doesn't use Zed's CI.

#### 2. `crates/title_bar/Cargo.toml`
**Upstream change**: removed `feature_flags.workspace = true` (Skills feature flag was the only consumer, removed by `2e70059cd9`).
**HEAD change**: Helix added `external_websocket_sync = { workspace = true, optional = true }` (cfg-gated WS connection-status icon).
**Resolution**: kept Helix `external_websocket_sync` line; dropped `feature_flags.workspace = true` (no remaining consumer in `title_bar.rs`).
**Risk**: none.

#### 3. `crates/title_bar/src/title_bar.rs` — `render_restricted_mode`
**Upstream change**: `TrustedWorktrees::has_restricted_worktrees` became a free function taking the worktree_store directly (no `try_get_global`, no `read(cx)`).
**HEAD change**: Helix added a cfg-gated `if cfg!(feature = "external_websocket_sync") { return None; }` at the top — Helix auto-trusts every worktree so the pill is meaningless.
**Resolution**: kept Helix's early-return; adopted upstream's new API for the body underneath.
**Risk**: none.

#### 4. `crates/project/src/agent_server_store.rs` — `reregister_agents` destructure
**Upstream change**: `c84c22dab5` "Deprecate ACP extensions" removed the `extension_agents` field from `AgentServerStoreState::Local`; switched to `..` in the destructure pattern.
**HEAD change**: Helix HEAD destructured three named fields (`extension_agents`, `_subscriptions`, `registry_subscribed`).
**Resolution**: dropped `extension_agents` (no longer a field anywhere); kept `_subscriptions` and `registry_subscribed` (still referenced in the body below); added trailing `..` for forward-compat.
**Risk**: none — compile-driven; if any remaining body code references the removed field it will fail to build.

#### 5. `crates/agent_ui/src/agent_panel.rs` — `load_panel` thread-restoration logic
**Upstream change**: PR `589dc95c87` "Restore last active agent panel entry" (#57150) rewrote thread restoration: introduces `thread_to_restore` (with `has_open_project && terminal_to_restore.is_none()` guard, primary `thread_id` lookup with `session_id` fallback, archived filtering, await on the metadata-store reload task); calls `panel.load_agent_thread(...)` with `thread_id` instead of `session_id`; adds `panel.restore_new_draft(new_draft_thread_id, ...)` for restoring draft UI state.
**HEAD change**: Helix HEAD had its own restoration path: WebSocket-wait at start; session-id-based `is_restorable` check; draft-prompt resurrection via `create_agent_thread(..., initial_content)` with `panel.draft_thread = Some(...)` and conditional `set_base_view`; `send_agent_ready` in the no-restore branch to unblock the server queue.
**Resolution**: kept Helix's `wait_for_websocket_connected` at the top (must precede ANY thread restoration so the agent_ready→open_thread handshake can complete). Adopted upstream's `thread_to_restore` (strictly more robust — terminal/thread exclusivity, primary+fallback lookup, archived filter). Adopted upstream's `panel.load_agent_thread(thread_id, ...)` call site. Adopted upstream's `restore_new_draft` for `new_draft_thread_id` (subsumes Helix's `draft_prompt`/`was_draft_active` logic; under `external_websocket_sync` the draft path is suppressed by Fix 1b anyway). **Kept Helix's `send_agent_ready` in the new `else` branch (when neither terminal nor thread restored) — critical: without it, the WS server waits 60s for agent_ready and the user perceives a stuck session.**
**Risk**: medium. The Helix WS-wait-then-restore flow is functionally preserved but now goes through upstream's new helper. Validation: E2E Phase 1 (basic creation), Phase 7 (open_thread), Phase 12 (reconnect) all exercise the panel-load + restore path.

#### 6. `crates/agent_ui/src/agent_panel.rs` — `load_agent_thread` entity-identity guard (Critical Fix #11)
**Upstream change**: PR `589dc95c87` changed `load_agent_thread` to take `thread_id: ThreadId` (not `session_id: SessionId`); rewrote the dedup to compare `conversation_view.read(cx).thread_id == thread_id`.
**HEAD change**: Helix Critical Fix #11 had a `#[cfg(feature = "external_websocket_sync")]` entity-identity guard at the top that called `external_websocket_sync::get_thread(&session_id.to_string())` — but `session_id` is no longer a parameter.
**Resolution**: prepended a `ThreadMetadataStore::try_global(cx).read(cx).entry(thread_id).and_then(|e| e.session_id.clone())` lookup. If the thread has an ACP session_id (i.e. it's been registered with the server), do the Helix entity-identity dance; otherwise fall through to upstream's thread_id-based dedup. Drafts that don't yet have a session_id naturally skip the guard.
**Risk**: medium. The guard's behavior is preserved for the bug it was designed to catch (sidebar split-brain on click of the currently-open thread). Validation: regression-test by clicking the active thread in the sidebar after a fresh container start.

#### 7. `crates/agent_ui/src/agent_panel.rs` — `ensure_thread_initialized` (Helix Fix 1b)
**Upstream change**: PR `589dc95c87` rewrote `ensure_thread_initialized` body — was a single `self.activate_draft(...)` call, now branches on `self.pending_terminal_spawn`, `self.should_create_terminal_for_new_entry(cx)` (deferred terminal spawn via `cx.defer_in`), else falls through to `activate_draft`. Also added `create_initial_terminal` and `spawn_initial_terminal` helpers.
**HEAD change**: Helix PR #56 Fix 1b had a `#[cfg(feature = "external_websocket_sync")] { return; }` guard inside the `BaseView::Uninitialized` branch to prevent speculative draft Claude spawn.
**Resolution**: kept the Helix cfg-gated `return;` as the **first statement** inside `if matches!(BaseView::Uninitialized)`, BEFORE the new terminal-spawn branches. This is critical — upstream's new terminal-spawn path also calls `connection.new_session()` indirectly via `spawn_terminal`, which would re-introduce the duplicate-Claude bug Fix 1b was created to prevent. Adopted upstream's terminal-spawn branches and new helper functions verbatim for the non-WS-sync build. Also adopted upstream's signature change for `activate_draft` (string `"agent_panel"` → enum `AgentThreadSource::AgentPanel`).
**Risk**: HIGH. This is the regression we explicitly planned for. Validation: **E2E Phase 17 (live Claude process count == real thread count) is the hard gate**. If Phase 17 fails for either `zed-agent` or `claude`, this resolution lost the suppression.

### Pre-existing Breakage Repaired (002029)

#### `crates/agent_servers/src/acp.rs` — `acquire_session_creation_slot` debug-label `cwd` binding
**Upstream change**: PR `c3951af24f` "Support additional session directories" removed the local `cwd: PathBuf` binding from `open_or_create_session` and `new_session` (now uses `directories: SessionDirectories`).
**HEAD change**: PR #50 `acquire_session_creation_slot` log-label format strings reference the removed `cwd` binding.
**Resolution**: changed both format strings to use `directories.cwd.display()` instead.
**Risk**: none — debug labels only.

#### `crates/agent_ui/src/conversation_view.rs` — `from_existing_thread` signature drift
**Upstream change**: PR `589dc95c87` "Restore last active agent panel entry" added `root_thread_id: ThreadId` as the first parameter of `ThreadView::new`; PR added `draft_prompt_persist_task: Option<Task<()>>` and `last_theme_id: Option<String>` to `ConversationView`. Independently, `SessionCapabilities::new` now takes three arguments (added `available_skills: Vec<AvailableSkill>`) — from the agent-skills work.
**HEAD change**: Helix `from_existing_thread` constructor was bound to the older 2-arg `SessionCapabilities::new`, the 23-arg `ThreadView::new`, and didn't initialise the new struct fields.
**Resolution**: added `vec![]` for `available_skills`; hoisted `let root_thread_id = ThreadId::new();` before the `ThreadView::new` call and passed it as the first arg; set `thread_id: root_thread_id` and added `last_theme_id: Some(cx.theme().id.clone())`, `draft_prompt_persist_task: None` to the `Self { ... }` literal.
**Risk**: low — UI-only fields with safe defaults. `last_theme_id` may force a redundant first re-render on a theme reload, but that's an existing pattern across the codebase.

#### `crates/agent_ui/src/agent_panel.rs` and `crates/zed/src/main.rs` — `ContextServerStatus` exhaustive match
**Upstream change**: upstream MCP work added `ContextServerStatus::ClientSecretRequired { .. }` variant.
**HEAD change**: two Helix UI-state-query loops (agent_panel.rs UI state callback; main.rs headless responder) matched the prior 6 variants exhaustively, no wildcard.
**Resolution**: added a `ClientSecretRequired { .. } => "client_secret_required"` arm to both. Reports the active state as a known short string (consistent with the other variants).
**Risk**: none. Helix server is forward-compatible — it doesn't enumerate the strings, just records them.
**Lesson for future merges**: same lesson as 001996's `BaseView::Terminal` repair — when upstream adds a variant to an enum the Helix code matches exhaustively, the silent-drift sweep doesn't catch it. Build-driven discovery is the only safety net.

### Notes on other upstream changes that did NOT require Helix action (002029)

#### `c84c22dab5` "Deprecate ACP extensions" — Helix bypass markers retained
The 80-line deletion in `extensions_ui.rs` reshaped the surrounding code but did NOT remove the lines Helix's `// HELIX: External agent ...` comments guard. Markers still present at lines 221, 243, 1513 — keep them as documentation of Helix's intent (no agent keywords / no upsells visible to corporate-LLM users).

#### `f2df3f9e18` "ACP logout" — no Helix override needed
Upstream's default impls (`supports_logout() -> false`, `logout() -> Err("Logout is not supported")`) are correct for Helix mode. No Helix-mode `AcpConnection` impl overrides them. UI gates the logout button on `supports_logout(cx)` so nothing surfaces in Helix builds. Confirm visually in the next user-facing change to the agent panel.

#### `supports_delete(&self)` → `supports_delete(&self, &App)` signature change (`23231879cd`)
Trait signature migration applied at 4 sites: trait default impl (`acp_thread/src/connection.rs:335`), upstream impl on NativeAgentConnection (`agent/src/agent.rs:2520`), upstream impl on AcpConnection (`agent_servers/src/acp.rs:558`), Helix UI wrapper on AcpThreadHistory (`agent_ui/src/acp/thread_history.rs:362`). Compile-driven; all call sites updated in a single sweep.
