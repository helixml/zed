// slow-mcp-server is a minimal MCP (Model Context Protocol) stdio server that
// deliberately delays its response to tools/list. It verifies that Zed's
// wait_for_tools_ready() logic works — the first LLM request should not fire
// until this server has finished loading its tools.
//
// Protocol: line-delimited JSON-RPC 2.0 over stdin/stdout.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"time"
)

const toolsDelay = 10 * time.Second

type jsonrpcRequest struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

type jsonrpcResponse struct {
	JSONRPC string      `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Result  interface{} `json:"result,omitempty"`
	Error   interface{} `json:"error,omitempty"`
}

func send(resp jsonrpcResponse) {
	data, err := json.Marshal(resp)
	if err != nil {
		fmt.Fprintf(os.Stderr, "[slow-mcp] Marshal error: %v\n", err)
		return
	}
	fmt.Fprintf(os.Stdout, "%s\n", data)
}

func handleRequest(req jsonrpcRequest) {
	switch req.Method {
	case "initialize":
		send(jsonrpcResponse{
			JSONRPC: "2.0",
			ID:      req.ID,
			Result: map[string]interface{}{
				"protocolVersion": "2024-11-05",
				"capabilities": map[string]interface{}{
					"tools": map[string]interface{}{
						"listChanged": false,
					},
				},
				"serverInfo": map[string]interface{}{
					"name":    "slow-mcp-test-server",
					"version": "1.0.0",
				},
			},
		})

	case "notifications/initialized":
		// No response for notifications

	case "tools/list":
		fmt.Fprintf(os.Stderr, "[slow-mcp] Delaying tools/list response by %s...\n", toolsDelay)
		time.Sleep(toolsDelay)
		fmt.Fprintf(os.Stderr, "[slow-mcp] Sending tools/list response now.\n")
		send(jsonrpcResponse{
			JSONRPC: "2.0",
			ID:      req.ID,
			Result: map[string]interface{}{
				"tools": []map[string]interface{}{
					{
						"name":        "slow_mcp_test_tool",
						"description": "A dummy tool from the slow MCP test server. Its presence proves tools were loaded before the LLM request.",
						"inputSchema": map[string]interface{}{
							"type":       "object",
							"properties": map[string]interface{}{},
						},
					},
				},
			},
		})

	case "tools/call":
		send(jsonrpcResponse{
			JSONRPC: "2.0",
			ID:      req.ID,
			Result: map[string]interface{}{
				"content": []map[string]interface{}{
					{"type": "text", "text": "slow_mcp_test_tool executed"},
				},
				"isError": false,
			},
		})

	case "ping":
		send(jsonrpcResponse{
			JSONRPC: "2.0",
			ID:      req.ID,
			Result:  map[string]interface{}{},
		})

	default:
		if req.ID != nil {
			send(jsonrpcResponse{
				JSONRPC: "2.0",
				ID:      req.ID,
				Error: map[string]interface{}{
					"code":    -32601,
					"message": fmt.Sprintf("Method not found: %s", req.Method),
				},
			})
		}
	}
}

func main() {
	fmt.Fprintf(os.Stderr, "[slow-mcp] Server started, reading from stdin...\n")

	scanner := bufio.NewScanner(os.Stdin)
	// Increase buffer size for large messages
	scanner.Buffer(make([]byte, 1024*1024), 1024*1024)

	for scanner.Scan() {
		line := scanner.Text()
		if line == "" {
			continue
		}

		var req jsonrpcRequest
		if err := json.Unmarshal([]byte(line), &req); err != nil {
			fmt.Fprintf(os.Stderr, "[slow-mcp] Invalid JSON: %s\n", line)
			continue
		}

		handleRequest(req)
	}

	if err := scanner.Err(); err != nil {
		fmt.Fprintf(os.Stderr, "[slow-mcp] Scanner error: %v\n", err)
	}
}
