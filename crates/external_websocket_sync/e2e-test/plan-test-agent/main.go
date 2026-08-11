// plan-test-agent is a deterministic ACP agent used by the headless Zed E2E
// suite. Its first turn publishes two plan snapshots; its second turn publishes
// no plan. This makes plan replacement and turn isolation testable without an
// external model or nondeterministic prompting.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
)

type request struct {
	JSONRPC string                 `json:"jsonrpc"`
	ID      json.RawMessage        `json:"id,omitempty"`
	Method  string                 `json:"method"`
	Params  map[string]interface{} `json:"params,omitempty"`
}

var promptCount int

func write(value interface{}) {
	encoded, err := json.Marshal(value)
	if err != nil {
		fmt.Fprintf(os.Stderr, "[plan-test-agent] marshal: %v\n", err)
		return
	}
	fmt.Fprintln(os.Stdout, string(encoded))
}

func respond(id json.RawMessage, result interface{}) {
	write(map[string]interface{}{
		"jsonrpc": "2.0",
		"id":      id,
		"result":  result,
	})
}

func notify(sessionID string, update map[string]interface{}) {
	write(map[string]interface{}{
		"jsonrpc": "2.0",
		"method":  "session/update",
		"params": map[string]interface{}{
			"sessionId": sessionID,
			"update":    update,
		},
	})
}

func planEntry(content, status string) map[string]interface{} {
	return map[string]interface{}{
		"content":  content,
		"priority": "medium",
		"status":   status,
	}
}

func publishPlan(sessionID string, entries ...map[string]interface{}) {
	notify(sessionID, map[string]interface{}{
		"sessionUpdate": "plan",
		"entries":       entries,
	})
}

func publishText(sessionID, text string) {
	notify(sessionID, map[string]interface{}{
		"sessionUpdate": "agent_message_chunk",
		"content": map[string]interface{}{
			"type": "text",
			"text": text,
		},
	})
}

func stringParam(params map[string]interface{}, key, fallback string) string {
	if value, ok := params[key].(string); ok && value != "" {
		return value
	}
	return fallback
}

func handle(req request) {
	switch req.Method {
	case "initialize":
		protocolVersion := req.Params["protocolVersion"]
		if protocolVersion == nil {
			protocolVersion = 1
		}
		respond(req.ID, map[string]interface{}{
			"protocolVersion":   protocolVersion,
			"agentCapabilities": map[string]interface{}{},
			"agentInfo": map[string]interface{}{
				"name":    "helix-plan-test-agent",
				"version": "1.0.0",
			},
		})
	case "session/new":
		respond(req.ID, map[string]interface{}{"sessionId": "plan-test-session"})
	case "session/prompt":
		promptCount++
		sessionID := stringParam(req.Params, "sessionId", "plan-test-session")
		if promptCount == 1 {
			publishPlan(sessionID,
				planEntry("Inspect the plan pipeline", "in_progress"),
				planEntry("Verify turn isolation", "pending"),
			)
			publishPlan(sessionID,
				planEntry("Inspect the plan pipeline", "completed"),
				planEntry("Verify turn isolation", "in_progress"),
			)
			publishText(sessionID, "First turn complete.")
		} else {
			publishText(sessionID, "Second turn has no plan.")
		}
		respond(req.ID, map[string]interface{}{"stopReason": "end_turn"})
	case "session/cancel":
		// Notifications have no response.
	default:
		if len(req.ID) > 0 {
			write(map[string]interface{}{
				"jsonrpc": "2.0",
				"id":      req.ID,
				"error": map[string]interface{}{
					"code":    -32601,
					"message": "method not found",
				},
			})
		}
	}
}

func main() {
	scanner := bufio.NewScanner(os.Stdin)
	for scanner.Scan() {
		var req request
		if err := json.Unmarshal(scanner.Bytes(), &req); err != nil {
			fmt.Fprintf(os.Stderr, "[plan-test-agent] decode: %v\n", err)
			continue
		}
		handle(req)
	}
	if err := scanner.Err(); err != nil {
		fmt.Fprintf(os.Stderr, "[plan-test-agent] stdin: %v\n", err)
	}
}
