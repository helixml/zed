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
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

type request struct {
	JSONRPC string                 `json:"jsonrpc"`
	ID      json.RawMessage        `json:"id,omitempty"`
	Method  string                 `json:"method"`
	Params  map[string]interface{} `json:"params,omitempty"`
}

var (
	promptCount    int
	sessionCount   atomic.Int64
	lifecycleCount atomic.Int64
	writeMu        sync.Mutex
	cancelMu       sync.Mutex
	cancels        = make(map[string]chan struct{})
	mcpMu          sync.Mutex
	// mcpServerNames is what session/new handed us. An ACP agent is given its
	// MCP servers once, at session creation, so this is the only chance to see
	// them — and reporting them is how the suite catches a Zed that creates the
	// session before the MCP settings are resolved.
	mcpServerNames []string
)

// recordMcpServers remembers the server names in a session/new or session/load
// request.
func recordMcpServers(params map[string]interface{}) {
	servers, _ := params["mcpServers"].([]interface{})
	names := make([]string, 0, len(servers))
	for _, entry := range servers {
		server, _ := entry.(map[string]interface{})
		if name, _ := server["name"].(string); name != "" {
			names = append(names, name)
		}
	}
	mcpMu.Lock()
	mcpServerNames = names
	mcpMu.Unlock()
}

// mcpReport is the line the suite asserts on.
func mcpReport() string {
	mcpMu.Lock()
	defer mcpMu.Unlock()
	if len(mcpServerNames) == 0 {
		return "MCP_SERVERS: none"
	}
	return "MCP_SERVERS: " + strings.Join(mcpServerNames, ",")
}

func write(value interface{}) {
	encoded, err := json.Marshal(value)
	if err != nil {
		fmt.Fprintf(os.Stderr, "[plan-test-agent] marshal: %v\n", err)
		return
	}
	writeMu.Lock()
	defer writeMu.Unlock()
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
			"agentCapabilities": map[string]interface{}{"loadSession": true},
			"agentInfo": map[string]interface{}{
				"name":    "helix-plan-test-agent",
				"version": "1.0.0",
			},
		})
	case "session/new":
		recordMcpServers(req.Params)
		sessionID := fmt.Sprintf("plan-test-session-%d-%d", os.Getpid(), sessionCount.Add(1))
		respond(req.ID, map[string]interface{}{"sessionId": sessionID})
	case "session/load":
		recordMcpServers(req.Params)
		respond(req.ID, map[string]interface{}{})
	case "session/prompt":
		if os.Getenv("E2E_SCRIPTED_LIFECYCLE") == "1" {
			runLifecyclePrompt(req)
			return
		}
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
			publishText(sessionID, mcpReport())
			publishText(sessionID, "First turn complete.")
		} else {
			publishText(sessionID, "Second turn has no plan.")
		}
		respond(req.ID, map[string]interface{}{"stopReason": "end_turn"})
	case "session/cancel":
		sessionID := stringParam(req.Params, "sessionId", "")
		cancelMu.Lock()
		cancel := cancels[sessionID]
		delete(cancels, sessionID)
		cancelMu.Unlock()
		if cancel != nil {
			close(cancel)
		}
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

// longTurnCue marks a prompt whose turn must still be streaming when the
// lifecycle driver acts on it: an interrupt or cancel aimed mid-turn, a message
// queued behind a busy session, or the streaming-cadence measurement. Every
// other prompt gets a short turn, so the suite spends no time on turns it only
// needs to complete. Must match lifecycleLongTurnCue in helix-ws-test-server.
const longTurnCue = "[e2e:long-turn]"

// Turn shapes. A long turn is cancellable at every chunk, so turns the driver
// interrupts end as soon as the cancel lands; only uninterrupted long turns
// pay the full longTurnChunks*longTurnChunkGap (2.5s), which is ample margin
// for a driver that reacts to the first streamed chunk within milliseconds.
const (
	shortTurnChunks  = 3
	longTurnChunks   = 25
	longTurnChunkGap = 100 * time.Millisecond
)

// promptText concatenates the text blocks of a session/prompt request.
func promptText(params map[string]interface{}) string {
	blocks, _ := params["prompt"].([]interface{})
	var text strings.Builder
	for _, entry := range blocks {
		block, _ := entry.(map[string]interface{})
		if value, _ := block["text"].(string); value != "" {
			text.WriteString(value)
		}
	}
	return text.String()
}

func runLifecyclePrompt(req request) {
	sessionID := stringParam(req.Params, "sessionId", "")
	turn := lifecycleCount.Add(1)
	cancel := make(chan struct{})
	cancelMu.Lock()
	if previous := cancels[sessionID]; previous != nil {
		close(previous)
	}
	cancels[sessionID] = cancel
	cancelMu.Unlock()

	chunks, gap := shortTurnChunks, time.Duration(0)
	if strings.Contains(promptText(req.Params), longTurnCue) {
		chunks, gap = longTurnChunks, longTurnChunkGap
	}

	go func() {
		respond(req.ID, map[string]interface{}{"stopReason": streamTurn(sessionID, turn, chunks, gap, cancel)})
		cancelMu.Lock()
		if cancels[sessionID] == cancel {
			delete(cancels, sessionID)
		}
		cancelMu.Unlock()
	}()
}

// streamTurn publishes chunks text chunks gap apart and returns the ACP stop
// reason: "cancelled" as soon as cancel closes, "end_turn" otherwise.
func streamTurn(sessionID string, turn int64, chunks int, gap time.Duration, cancel <-chan struct{}) string {
	for i := 1; i <= chunks; i++ {
		select {
		case <-cancel:
			return "cancelled"
		default:
		}
		publishText(sessionID, fmt.Sprintf("scripted turn %02d chunk %02d. ", turn, i))
		if i == chunks || gap == 0 {
			continue
		}
		select {
		case <-cancel:
			return "cancelled"
		case <-time.After(gap):
		}
	}
	return "end_turn"
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
