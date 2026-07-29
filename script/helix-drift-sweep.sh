#!/usr/bin/env bash
# Helix fork silent-drift sweep.
# Verifies every Helix critical fix / load-bearing patch is still present after
# an upstream merge. Derived from portingguide.md "Rebase Checklist".
# Usage: ./helix-drift-sweep.sh [path-to-zed-repo]
cd "${1:-$HOME/pm/zed}" || exit 2

pass=0; fail=0
# chk <label> <expected-min-count> <grep-args...>
chk() {
  local label="$1" min="$2"; shift 2
  local n; n=$(grep -rc "$@" 2>/dev/null | awk -F: '{s+=$NF} END{print s+0}')
  if [ "$n" -ge "$min" ]; then
    printf '  OK   %-58s (%d)\n' "$label" "$n"; pass=$((pass+1))
  else
    printf '  FAIL %-58s (%d, want >=%d)\n' "$label" "$n" "$min"; fail=$((fail+1))
  fi
}
# absent <label> <grep-args...>  — must NOT appear
absent() {
  local label="$1"; shift
  local n; n=$(grep -rc "$@" 2>/dev/null | awk -F: '{s+=$NF} END{print s+0}')
  if [ "$n" -eq 0 ]; then
    printf '  OK   %-58s (absent)\n' "$label"; pass=$((pass+1))
  else
    printf '  FAIL %-58s (%d hits, must be 0)\n' "$label" "$n"; fail=$((fail+1))
  fi
}

echo "== Critical Fixes =="
chk "#1  load_session pending_sessions"      1 -e "pending_sessions"           crates/agent/src/agent.rs
chk "#3  content_only()"                     1 -e "fn content_only"            crates/acp_thread/src/acp_thread.rs
chk "#6/#9 stopped_emitted_for_task"         3 -e "stopped_emitted_for_task"   crates/acp_thread/src/acp_thread.rs
chk "#8  drop(turn.send_task)"               1 -e "drop(turn.send_task)"       crates/acp_thread/src/acp_thread.rs
chk "#11 entity-identity guard (get_thread)" 1 -e "external_websocket_sync::get_thread" crates/agent_ui/src/agent_panel.rs

echo "== Helix PR surface =="
chk "#50 session_creation_chain"             1 -e "session_creation_chain"     crates/agent_servers/src/acp.rs
chk "#55 EntryUpdated emit"                  8 -e "AcpThreadEvent::EntryUpdated" crates/acp_thread/src/acp_thread.rs
chk "#56 1a deferred UserCreatedThread"      1 -e "UserCreatedThread"          crates/external_websocket_sync/src/thread_service.rs
chk "#60 ede_diagnostic retry"               1 -e "ede_diagnostic"             crates/external_websocket_sync/src/thread_service.rs

echo "== Modified upstream files =="
chk "AcpBetaFeatureFlag enabled_for_all"     1 -e "fn enabled_for_all"         crates/feature_flags/src/flags.rs
chk "HELIX: external-agent markers"          3 -e "HELIX:"                     crates/extensions_ui/src/extensions_ui.rs
chk "title_bar render_restricted_mode"       1 -e "render_restricted_mode"     crates/title_bar/src/title_bar.rs
chk "main.rs --headless"                     1 -e "headless"                   crates/zed/src/main.rs
chk "main.rs --allow-multiple-instances"     1 -e "allow_multiple_instances"   crates/zed/src/main.rs
chk "rust-embed debug-embed"                 1 -e "debug-embed"                Cargo.toml
chk "workspace.rs Agent no-focus-steal"      1 -e "CollaboratorId::Agent"      crates/workspace/src/workspace.rs
chk "migrate.rs Hidden in Helix"             1 -e "external_websocket_sync"    crates/zed/src/zed/migrate.rs
chk "grep_tool truncate_long_lines"          1 -e "truncate_long_lines"        crates/agent/src/tools/grep_tool.rs
chk "config_options current_model_value"     1 -e "current_model_value"        crates/agent_ui/src/config_options.rs
chk "dev_container suggest guard"            1 -e "suggest_dev_container"      crates/recent_projects/src/dev_container_suggest.rs
chk "http_client_tls NoCertVerifier"         1 -e "NoCertVerifier"             crates/http_client_tls/src/http_client_tls.rs
chk "reqwest insecure TLS"                   1 -e "ZED_HTTP_INSECURE_TLS"      crates/reqwest_client/src/reqwest_client.rs
chk "agent_settings show_onboarding"         1 -e "show_onboarding"            crates/agent_settings/src/agent_settings.rs
chk "external_websocket_sync in workspace"   1 -e "external_websocket_sync"    Cargo.toml
chk "open_ai chat reasoning effort (135f5b4)" 1 -e "chat_completions_reasoning_effort(&self.model)" crates/language_models/src/provider/open_ai.rs
chk "reqwest insecure-TLS branch is tail"    1 -e "is_insecure_tls_enabled()"  crates/reqwest_client/src/reqwest_client.rs

echo "== Known regressions (must stay absent) =="
absent "smol::Timer in agent crate"            -e "smol::Timer"                crates/agent/src/
# Real unit-variant *uses* only (match arm / matches! / comparison) — not prose comments.
absent "unit-variant Stopped pattern"          -En "AcpThreadEvent::Stopped\s*(=>|\)|,|\||$)" crates/acp_thread/src/

echo
echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
