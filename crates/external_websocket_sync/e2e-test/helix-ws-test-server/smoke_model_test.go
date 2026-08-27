package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func smokeRequest(t *testing.T, messages []map[string]interface{}) *http.Request {
	t.Helper()
	body, err := json.Marshal(map[string]interface{}{
		"model":    "e2e-smoke-model",
		"messages": messages,
		"stream":   true,
		"tools": []interface{}{map[string]interface{}{
			"type": "function",
			"function": map[string]interface{}{
				"name": "read_file",
			},
		}},
	})
	if err != nil {
		t.Fatalf("marshal request: %v", err)
	}
	return httptest.NewRequest(http.MethodPost, "/v1/chat/completions", bytes.NewReader(body))
}

func smokeEvents(t *testing.T, body string) []map[string]interface{} {
	t.Helper()
	var events []map[string]interface{}
	scanner := bufio.NewScanner(strings.NewReader(body))
	for scanner.Scan() {
		line := scanner.Text()
		if !strings.HasPrefix(line, "data: ") || line == "data: [DONE]" {
			continue
		}
		var event map[string]interface{}
		if err := json.Unmarshal([]byte(strings.TrimPrefix(line, "data: ")), &event); err != nil {
			t.Fatalf("decode SSE event: %v", err)
		}
		events = append(events, event)
	}
	if err := scanner.Err(); err != nil {
		t.Fatalf("scan SSE response: %v", err)
	}
	return events
}

func TestSmokeChatCompletionRequestsReadFile(t *testing.T) {
	recorder := httptest.NewRecorder()
	smokeChatCompletionHandler(recorder, smokeRequest(t, []map[string]interface{}{
		{"role": "user", "content": "read the fixture"},
	}))

	if recorder.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", recorder.Code, recorder.Body.String())
	}
	if contentType := recorder.Header().Get("Content-Type"); contentType != "text/event-stream" {
		t.Fatalf("Content-Type = %q", contentType)
	}
	events := smokeEvents(t, recorder.Body.String())
	if len(events) != 2 {
		t.Fatalf("got %d events, body = %s", len(events), recorder.Body.String())
	}
	encoded, err := json.Marshal(events[0])
	if err != nil {
		t.Fatalf("marshal response event: %v", err)
	}
	response := string(encoded)
	for _, expected := range []string{"call_e2e_read_file", "read_file", smokeMagicFile} {
		if !strings.Contains(response, expected) {
			t.Errorf("first response event does not contain %q: %s", expected, response)
		}
	}
}

func TestSmokeChatCompletionReturnsToolResult(t *testing.T) {
	recorder := httptest.NewRecorder()
	smokeChatCompletionHandler(recorder, smokeRequest(t, []map[string]interface{}{
		{"role": "user", "content": "read the fixture"},
		{"role": "tool", "content": "1→4242"},
	}))

	if recorder.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", recorder.Code, recorder.Body.String())
	}
	events := smokeEvents(t, recorder.Body.String())
	if len(events) != 2 {
		t.Fatalf("got %d events, body = %s", len(events), recorder.Body.String())
	}
	encoded, err := json.Marshal(events[0])
	if err != nil {
		t.Fatalf("marshal response event: %v", err)
	}
	if !strings.Contains(string(encoded), smokeMagicValue) {
		t.Fatalf("response does not contain %q: %s", smokeMagicValue, encoded)
	}
}

func TestSmokeChatCompletionRejectsWrongToolResult(t *testing.T) {
	recorder := httptest.NewRecorder()
	smokeChatCompletionHandler(recorder, smokeRequest(t, []map[string]interface{}{
		{"role": "tool", "content": "wrong value"},
	}))

	if recorder.Code != http.StatusBadRequest {
		t.Fatalf("status = %d, body = %s", recorder.Code, recorder.Body.String())
	}
}
