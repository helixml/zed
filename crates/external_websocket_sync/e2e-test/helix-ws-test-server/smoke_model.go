package main

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"strings"
)

type smokeChatCompletionRequest struct {
	Model    string `json:"model"`
	Messages []struct {
		Role    string          `json:"role"`
		Content json.RawMessage `json:"content"`
	} `json:"messages"`
	Stream bool `json:"stream"`
	Tools  []struct {
		Function struct {
			Name string `json:"name"`
		} `json:"function"`
	} `json:"tools"`
}

type smokeChatCompletionEvent struct {
	Choices []smokeChatCompletionChoice `json:"choices"`
	Usage   *smokeChatCompletionUsage   `json:"usage"`
}

type smokeChatCompletionUsage struct{}

type smokeChatCompletionChoice struct {
	Index        int                       `json:"index"`
	Delta        *smokeChatCompletionDelta `json:"delta"`
	FinishReason *string                   `json:"finish_reason,omitempty"`
}

type smokeChatCompletionDelta struct {
	Role      string                        `json:"role,omitempty"`
	Content   string                        `json:"content,omitempty"`
	ToolCalls []smokeChatCompletionToolCall `json:"tool_calls,omitempty"`
}

type smokeChatCompletionToolCall struct {
	Index    int                                `json:"index"`
	ID       string                             `json:"id"`
	Function smokeChatCompletionToolCallDetails `json:"function"`
}

type smokeChatCompletionToolCallDetails struct {
	Name      string `json:"name"`
	Arguments string `json:"arguments"`
}

func smokeChatCompletionHandler(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method must be POST", http.StatusMethodNotAllowed)
		return
	}

	var request smokeChatCompletionRequest
	if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
		http.Error(w, "invalid JSON request", http.StatusBadRequest)
		return
	}
	if request.Model != "e2e-smoke-model" {
		http.Error(w, "unexpected model", http.StatusBadRequest)
		return
	}
	if !request.Stream {
		http.Error(w, "streaming is required", http.StatusBadRequest)
		return
	}

	hasReadFileTool := false
	for _, tool := range request.Tools {
		if tool.Function.Name == "read_file" {
			hasReadFileTool = true
			break
		}
	}
	if !hasReadFileTool {
		http.Error(w, "read_file tool was not provided", http.StatusBadRequest)
		return
	}

	var toolResult json.RawMessage
	for _, message := range request.Messages {
		if message.Role == "tool" {
			toolResult = message.Content
		}
	}

	if toolResult == nil {
		arguments, err := json.Marshal(map[string]string{"path": smokeMagicFile})
		if err != nil {
			http.Error(w, "failed to encode tool arguments", http.StatusInternalServerError)
			return
		}
		w.Header().Set("Content-Type", "text/event-stream")
		w.Header().Set("Cache-Control", "no-cache")
		finishReason := "tool_calls"
		writeSmokeSSE(w,
			smokeChatCompletionEvent{Choices: []smokeChatCompletionChoice{{
				Index: 0,
				Delta: &smokeChatCompletionDelta{
					Role: "assistant",
					ToolCalls: []smokeChatCompletionToolCall{{
						Index: 0,
						ID:    "call_e2e_read_file",
						Function: smokeChatCompletionToolCallDetails{
							Name:      "read_file",
							Arguments: string(arguments),
						},
					}},
				},
			}}},
			smokeChatCompletionEvent{Choices: []smokeChatCompletionChoice{{
				Index:        0,
				FinishReason: &finishReason,
			}}},
		)
		return
	}

	if !strings.Contains(string(toolResult), smokeMagicValue) {
		http.Error(w, "read_file result did not contain the magic value", http.StatusBadRequest)
		return
	}
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	finishReason := "stop"
	writeSmokeSSE(w,
		smokeChatCompletionEvent{Choices: []smokeChatCompletionChoice{{
			Index: 0,
			Delta: &smokeChatCompletionDelta{
				Role:    "assistant",
				Content: smokeMagicValue,
			},
		}}},
		smokeChatCompletionEvent{Choices: []smokeChatCompletionChoice{{
			Index:        0,
			FinishReason: &finishReason,
		}}},
	)
}

func writeSmokeSSE(w http.ResponseWriter, events ...smokeChatCompletionEvent) {
	for _, event := range events {
		encoded, err := json.Marshal(event)
		if err != nil {
			log.Printf("[smoke-model] Failed to encode event: %v", err)
			return
		}
		if _, err := fmt.Fprintf(w, "data: %s\n\n", encoded); err != nil {
			log.Printf("[smoke-model] Failed to write event: %v", err)
			return
		}
	}
	if _, err := fmt.Fprint(w, "data: [DONE]\n\n"); err != nil {
		log.Printf("[smoke-model] Failed to write stream terminator: %v", err)
	}
}
