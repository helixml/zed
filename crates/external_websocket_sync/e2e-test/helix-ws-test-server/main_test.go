package main

import (
	"testing"

	"github.com/helixml/helix/api/pkg/types"
)

func TestResponseEntryIsolationKey(t *testing.T) {
	previous := map[responseEntryKey]string{{messageID: "3", content: "first"}: "interaction-1"}
	if _, leaked := responseEntryLeak(previous, "3", "second"); leaked {
		t.Fatal("same message ID with different content must not be treated as leakage")
	}
	if owner, leaked := responseEntryLeak(previous, "3", "first"); !leaked || owner != "interaction-1" {
		t.Fatal("exact message ID and content reuse must be treated as leakage")
	}
}

func TestHasIntermediatePhase15Content(t *testing.T) {
	tests := []struct {
		name    string
		lengths []int
		final   int
		want    bool
	}{
		{name: "duplicate snapshots with progress", lengths: []int{0, 0, 4, 4, 1494, 2698, 2698}, final: 2698, want: true},
		{name: "build 4644 provider cadence", lengths: []int{0, 3, 346, 498, 2832}, final: 2832, want: true},
		{name: "only terminal content", lengths: []int{0, 0, 2698, 2698}, final: 2698, want: false},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			samples := make([]phase15AddSample, len(test.lengths))
			for i, length := range test.lengths {
				samples[i].contentLen = length
			}
			if got := hasIntermediatePhase15Content(samples, test.final); got != test.want {
				t.Fatalf("hasIntermediatePhase15Content() = %v, want %v", got, test.want)
			}
		})
	}
}

func TestIsCompletedSlowMCPToolCall(t *testing.T) {
	data := map[string]interface{}{
		"request_id": "req-phase1-claude", "entry_type": "tool_call",
		"tool_status": "Completed", "tool_name": "slow_mcp_test_tool",
	}
	if !isCompletedSlowMCPToolCall(data, "req-phase1-claude") {
		t.Fatal("completed slow MCP tool call was not recognized")
	}
	data["tool_status"] = "Failed"
	if isCompletedSlowMCPToolCall(data, "req-phase1-claude") {
		t.Fatal("failed slow MCP tool call was accepted")
	}
	data["tool_status"] = "Completed"
	data["request_id"] = "req-phase2-claude"
	if isCompletedSlowMCPToolCall(data, "req-phase1-claude") {
		t.Fatal("tool call from another turn was accepted")
	}
}

func TestStreamedBeforeCompletion(t *testing.T) {
	event := func(eventType, requestID, role string) types.SyncMessage {
		return types.SyncMessage{EventType: eventType, Data: map[string]interface{}{
			"request_id": requestID, "role": role, "entry_type": "text", "content": "chunk",
		}}
	}
	planSnapshot := types.SyncMessage{EventType: "message_added", Data: map[string]interface{}{
		"request_id": "req-2", "role": "assistant", "entry_type": "plan", "content": `{"steps":[]}`,
	}}
	tests := []struct {
		name   string
		events []types.SyncMessage
		want   int
	}{
		{
			name: "streamed before its completion",
			events: []types.SyncMessage{
				event("message_added", "req-2", "assistant"),
				event("message_added", "req-2", "assistant"),
				event("message_completed", "req-2", ""),
			},
			want: 2,
		},
		{
			// The bug this guards: another turn's earlier streaming must not
			// count for a turn whose content only arrived with its completion.
			name: "only another request streamed first",
			events: []types.SyncMessage{
				event("message_added", "req-1", "assistant"),
				event("message_completed", "req-1", ""),
				event("message_completed", "req-2", ""),
				event("message_added", "req-2", "assistant"),
			},
			want: 0,
		},
		{
			// Zed emits the plan snapshot as the turn starts, before the agent
			// has the prompt; it must not count as the turn streaming.
			name:   "plan snapshot is not agent output",
			events: []types.SyncMessage{planSnapshot, event("message_completed", "req-2", "")},
			want:   0,
		},
		{
			name: "user echo is not assistant streaming",
			events: []types.SyncMessage{
				event("message_added", "req-2", "user"),
				event("message_completed", "req-2", ""),
			},
			want: 0,
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := streamedBeforeCompletion(test.events, "req-2"); got != test.want {
				t.Fatalf("streamedBeforeCompletion() = %d, want %d", got, test.want)
			}
		})
	}
}
