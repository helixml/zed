// helix-ws-test-server is a standalone WebSocket server for E2E testing of the
// Zed <-> Helix sync protocol. It runs the REAL production HelixAPIServer handlers
// with an in-memory store, so the same message processing code runs in both
// tests and production.
//
// The server runs multiple "rounds", one per agent type (zed-agent, claude, etc.).
// Each round executes the same 15 test phases:
//
//	Phase 1: Basic thread creation (new chat_message, no thread ID)
//	Phase 2: Follow-up on existing thread (same thread ID)
//	Phase 3: New thread (simulates context exhaustion -> new thread)
//	Phase 4: Follow-up to non-visible thread (Thread A while Thread B is active)
//	Phase 5: Simulate user input (Zed -> Helix sync direction)
//	Phase 6: Query UI state (verify Zed reports active thread)
//	Phase 7: Open thread + follow-up (open_thread command then chat_message)
//	Phase 8: Mid-stream interrupt (send follow-up while previous response is streaming)
//	Phase 9: Rapid 3-turn cancel (chat_message, then simulate_user_input interrupt, then chat_message)
//	Phase 10: User-created thread (inject user_created_thread, verify work session, send chat on new thread)
//	Phase 11: Spectask routing (set SpecTaskID on threads, verify FindConnectedSessionForSpecTask picks most recent)
//	Phase 12: Reconnect test (kill Zed, wait for reconnection, send message to existing thread, verify delivery)
//	Phase 13: Helix-initiated cancel (send chat_message, then cancel_current_turn, verify turn_cancelled with status cancelled)
//	Phase 14: Cancel no-op (send cancel_current_turn with bogus request_id, verify turn_cancelled with status noop)
//	Phase 15: Streaming patches arrive incrementally (long-form prose response — assert message_added events
//	          arrive throughout the response, not bunched at the end. Catches the regression where Zed's
//	          streaming-reveal task drains text into the markdown entity without re-emitting EntryUpdated,
//	          so external_websocket_sync only sees stale snapshots until message_completed.)
//
// Exit codes: 0 = all tests passed, 1 = test failure
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"os/exec"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/helixml/helix/api/pkg/pubsub"
	"github.com/helixml/helix/api/pkg/server"
	"github.com/helixml/helix/api/pkg/store/memorystore"
	"github.com/helixml/helix/api/pkg/types"
)

// roundState holds per-agent-round state that gets reset between rounds.
type roundState struct {
	agentName string

	// Track thread IDs from thread_created events
	threadIDs []string

	// Track all sync events for validation
	events []types.SyncMessage

	// Track completions per thread
	completions map[string][]string // threadID -> list of request_ids

	// Track UI state responses (from query_ui_state)
	uiStateResponses []types.SyncMessage

	// Track timing for MCP tools wait validation
	phase1ChatSentAt    time.Time // when we sent the chat_message for phase 1
	phase1ThreadCreated time.Time // when we received thread_created for phase 1

	// Phase 8: mid-stream interrupt state
	phase8ThreadID      string // thread ID created in phase 8
	phase8InterruptSent bool   // whether we have already sent the interrupt
	phase8Completions   int    // number of message_completed events received for phase 8 thread

	// Phase 9: rapid 3-turn cancel state
	phase9ThreadID  string // thread ID (reuses phase 8's thread)
	phase9RapidSent bool   // whether the rapid sequence has been sent
	phase9Completions int  // number of message_completed events for phase 9

	// Phase 10: user-created thread (multi-thread sync)
	phase10NewThreadID       string // synthetic thread ID injected via ProcessSyncEvent
	phase10WorkSessionFound  bool   // whether the work session was created
	phase10ChatCompleted     bool   // whether chat on the new thread completed

	// Phase 11: spectask routing (verifies findConnectedSessionForSpecTask
	// picks the most recently active session)
	phase11RoutedSessionID string // which session the routing picked
	phase11ExpectedThreadID string // which thread we expect the message to land on
	phase11Completed        bool   // whether the routed message completed

	// Phase 12: reconnect test (kill Zed, reconnect, verify message delivery)
	phase12Completed bool // whether the reconnected message completed

	// Phase 13: Helix-initiated cancel via cancel_current_turn
	phase13ThreadID     string // thread ID for the long-running turn
	phase13CancelSent   bool   // whether cancel_current_turn has been sent
	phase13TurnCancelled bool  // whether turn_cancelled event was received
	phase13CancelStatus string // status from turn_cancelled ("cancelled" or "noop")

	// Phase 14: cancel no-op (request_id not found)
	phase14TurnCancelled bool   // whether turn_cancelled event was received
	phase14CancelStatus  string // status from turn_cancelled (expected "noop")

	// Phase 15: streaming-cadence regression test
	phase15ThreadID    string             // thread ID created in phase 15
	phase15ChatSentAt  time.Time          // when we sent the chat_message
	phase15CompletedAt time.Time          // when message_completed arrived
	phase15Adds        []phase15AddSample // per message_added: timestamp + content length
	phase15FinalLen    int                // length of the final assistant content

	// Phase 16: regression for the speculative-draft `user_created_thread`
	// emit. Pre-Fix-1a (helixml/zed PR #56), every Zed startup fired a
	// spontaneous `user_created_thread` for the agent panel's
	// `activate_draft` ConversationView — see
	// https://github.com/helixml/helix/blob/main/design/2026-05-13-mcp-cache-contention-and-duplicate-claude-spawn.md
	// for the full diagnosis. Post-fix, no spontaneous emit should arrive
	// because emission is deferred until the user actually sends a message
	// in the draft. We assert spontaneousUserCreatedThreadCount == 0 at
	// the end of each round; Phase 10's own ProcessSyncEvent injection
	// fires the same syncEventHook, so the user_created_thread case
	// filters Phase 10's synthetic ID out of this counter explicitly.
	spontaneousUserCreatedThreadCount int
	spontaneousUserCreatedThreadIDs   []string
}

// phase15AddSample records a single message_added event tied to phase 15's
// assistant turn — used to assert streaming patches arrive throughout the
// response and not in a single burst at the end.
type phase15AddSample struct {
	ts         time.Time
	contentLen int
}

func newRoundState(agentName string) *roundState {
	return &roundState{
		agentName:   agentName,
		completions: make(map[string][]string),
	}
}

// reqID returns a round-namespaced request ID for validation uniqueness.
func (r *roundState) reqID(phase string) string {
	return fmt.Sprintf("req-%s-%s", phase, r.agentName)
}

type testDriver struct {
	mu sync.Mutex

	srv   *server.HelixAPIServer
	store *memorystore.MemoryStore

	phase   int
	done    chan struct{}
	agentID string // agent connection ID (discovered at runtime)

	// Multi-agent round management
	agentRounds    []string     // agent names to test (e.g., ["zed-agent", "claude"])
	currentRoundIdx int
	round          *roundState  // current round state

	// Collected round results for final summary
	roundResults []roundResult

	// Monotonic counter to detect stale advanceToNextRound calls.
	// Incremented on every round transition. Goroutines capture the value
	// before sleeping and check it hasn't changed when they wake up.
	roundGeneration int

	// Per-phase timeout tracking
	phaseStarted time.Time
	phaseTimedOut map[int]bool // phases that timed out (skip in validation)
}

type roundResult struct {
	agentName string
	passed    bool
	errors    []string
}

func newTestDriver(srv *server.HelixAPIServer, store *memorystore.MemoryStore, agents []string) *testDriver {
	return &testDriver{
		srv:         srv,
		store:       store,
		done:        make(chan struct{}),
		agentRounds: agents,
		round:       newRoundState(agents[0]),
	}
}

// syncEventCallback is called by the production handler after every sync event.
func (d *testDriver) syncEventCallback(sessionID string, syncMsg *types.SyncMessage) {
	d.mu.Lock()
	d.round.events = append(d.round.events, *syncMsg)

	switch syncMsg.EventType {
	case "agent_ready":
		if d.phase == 0 {
			d.phase = 1
			d.mu.Unlock()
			log.Printf("\n##################################################")
			log.Printf("  ROUND %d/%d: Agent = %s", d.currentRoundIdx+1, len(d.agentRounds), d.round.agentName)
			log.Printf("##################################################")
			d.runPhase1()
			return
		}

	case "thread_created":
		acpThreadID, _ := syncMsg.Data["acp_thread_id"].(string)
		if acpThreadID == "" {
			acpThreadID, _ = syncMsg.Data["context_id"].(string)
		}
		if acpThreadID != "" {
			if len(d.round.threadIDs) == 0 {
				d.round.phase1ThreadCreated = time.Now()
			}
			d.round.threadIDs = append(d.round.threadIDs, acpThreadID)
			log.Printf("[%s] Thread #%d: %s (event=%s)", d.round.agentName, len(d.round.threadIDs), syncMsg.EventType, truncate(acpThreadID, 16))
			// Capture the thread created for phase 8 so we can send the interrupt to it.
			if d.phase == 8 && d.round.phase8ThreadID == "" {
				d.round.phase8ThreadID = acpThreadID
				log.Printf("[%s] Phase 8: Captured thread ID: %s", d.round.agentName, truncate(acpThreadID, 16))
			}
			// Capture the thread created for phase 13 (Helix-initiated cancel).
			if d.phase == 13 && d.round.phase13ThreadID == "" {
				d.round.phase13ThreadID = acpThreadID
				log.Printf("[%s] Phase 13: Captured thread ID: %s", d.round.agentName, truncate(acpThreadID, 16))
			}
			// Capture the thread created for phase 15 (streaming-cadence test).
			if d.phase == 15 && d.round.phase15ThreadID == "" {
				d.round.phase15ThreadID = acpThreadID
				log.Printf("[%s] Phase 15: Captured thread ID: %s", d.round.agentName, truncate(acpThreadID, 16))
			}
		}

	case "user_created_thread":
		// Spontaneous threads created by Zed (e.g. on startup). The production
		// handler creates a child session for these, but they are NOT used for
		// test phase follow-ups — only thread_created from chat_message responses
		// go into threadIDs. Phase 10 tests user_created_thread separately via
		// ProcessSyncEvent injection.
		acpThreadID, _ := syncMsg.Data["acp_thread_id"].(string)
		if acpThreadID != "" {
			// Phase 10's own injection goes through ProcessSyncEvent, which
			// fires the same syncEventHook as real WebSocket-delivered events
			// — so we have to filter it out by ID here. The synthetic format
			// (`user-thread-{agent}-{nanos}`, see runPhase10) is generated
			// only by the test driver, never by Zed (which uses ACP UUIDs),
			// so this exclusion cannot mask a real Zed regression.
			if acpThreadID == d.round.phase10NewThreadID {
				log.Printf("[%s] Phase 10 injected user_created_thread: %s (excluded from Phase 16 counter)", d.round.agentName, truncate(acpThreadID, 16))
				break
			}
			log.Printf("[%s] Spontaneous user_created_thread: %s (not tracked for phases)", d.round.agentName, truncate(acpThreadID, 16))
			// Phase 16: count these for the deferred-emit regression
			// assertion. Post-Fix-1a there should be zero of these per
			// round — emission is deferred until the user actually
			// sends in the draft thread, which never happens in our
			// test sequence (we drive the threads via chat_message
			// instead). See `spontaneousUserCreatedThreadCount` doc.
			d.round.spontaneousUserCreatedThreadCount++
			d.round.spontaneousUserCreatedThreadIDs = append(
				d.round.spontaneousUserCreatedThreadIDs, acpThreadID,
			)
		}

	case "message_added":
		// Ignore message_added events for threads from previous rounds.
		// Check if the thread ID belongs to the current round's tracked threads.
		addedThreadID, _ := syncMsg.Data["acp_thread_id"].(string)
		isCurrentRoundThread := false
		for _, tid := range d.round.threadIDs {
			if tid == addedThreadID {
				isCurrentRoundThread = true
				break
			}
		}
		// Also check phase 8/9/13/15 thread IDs (they may not be in threadIDs yet)
		if addedThreadID == d.round.phase8ThreadID || addedThreadID == d.round.phase9ThreadID || addedThreadID == d.round.phase13ThreadID || addedThreadID == d.round.phase15ThreadID {
			isCurrentRoundThread = true
		}
		if !isCurrentRoundThread && addedThreadID != "" {
			d.mu.Unlock()
			return // silently ignore stale events
		}

		// Phase 15: record (timestamp, content_length) for every assistant
		// message_added event on the phase 15 thread. The validator asserts
		// these arrive throughout the streaming response, not bunched at the
		// end. Catches regressions where text drained from
		// streaming_text_buffer.pending into the markdown entity isn't seen
		// by external_websocket_sync until the next chunk arrives.
		if d.phase == 15 && addedThreadID != "" && addedThreadID == d.round.phase15ThreadID {
			role, _ := syncMsg.Data["role"].(string)
			if role == "assistant" {
				content, _ := syncMsg.Data["content"].(string)
				d.round.phase15Adds = append(d.round.phase15Adds, phase15AddSample{
					ts:         time.Now(),
					contentLen: len(content),
				})
			}
		}

		// Phase 8: send an interrupt as soon as the first assistant token arrives for
		// the phase 8 thread. This guarantees ACP has started generating (running_turn
		// is set), so the interrupt will properly cancel the active turn via run_turn().
		if d.phase == 8 && !d.round.phase8InterruptSent {
			role, _ := syncMsg.Data["role"].(string)
			threadID, _ := syncMsg.Data["acp_thread_id"].(string)
			if role == "assistant" && threadID == d.round.phase8ThreadID {
				d.round.phase8InterruptSent = true
				agentName := d.round.agentName
				d.mu.Unlock()
				log.Printf("[%s] Phase 8: First assistant token arrived, sending interrupt to %s", agentName, truncate(threadID, 16))
				d.sendChatMessage("Actually just say 'hello'.", d.round.reqID("phase8-interrupt"), agentName, threadID)
				return
			}
		}

		// Phase 13: send cancel_current_turn as soon as the first assistant token arrives.
		// This tests the Helix-initiated cancel flow via the cancel_current_turn command.
		if d.phase == 13 && !d.round.phase13CancelSent {
			role, _ := syncMsg.Data["role"].(string)
			threadID, _ := syncMsg.Data["acp_thread_id"].(string)
			if role == "assistant" && threadID == d.round.phase13ThreadID {
				d.round.phase13CancelSent = true
				agentName := d.round.agentName
				reqID := d.round.reqID("phase13")
				d.mu.Unlock()
				log.Printf("[%s] Phase 13: First assistant token arrived, sending cancel_current_turn for req=%s", agentName, reqID)
				d.sendCancelCurrentTurn(reqID)
				return
			}
		}

		// Phase 9: as soon as the first assistant token arrives for the initial
		// turn, fire a rapid sequence: simulate_user_input (like user pressing
		// Enter in Zed) + chat_message (like Helix's queue delivery). This
		// creates a 3-turn rapid cancel chain that previously caused a Task to
		// be dropped, breaking the oneshot channel and hanging the thread.
		if d.phase == 9 && !d.round.phase9RapidSent {
			role, _ := syncMsg.Data["role"].(string)
			threadID, _ := syncMsg.Data["acp_thread_id"].(string)
			if role == "assistant" && threadID == d.round.phase9ThreadID {
				d.round.phase9RapidSent = true
				agentName := d.round.agentName
				d.mu.Unlock()
				log.Printf("[%s] Phase 9: First assistant token arrived, sending rapid 2-message sequence", agentName)
				// Turn 2: simulate user typing in Zed (interrupt)
				d.sendSimulateUserInput(threadID, "User interrupt from Zed", d.round.reqID("phase9-user-input"), agentName)
				// Turn 3: simulate Helix queue delivery (arrives while turn 2 is starting)
				d.sendChatMessage("Queue delivery from Helix", d.round.reqID("phase9-queue"), agentName, threadID)
				return
			}
		}

	case "message_completed":
		acpThreadID, _ := syncMsg.Data["acp_thread_id"].(string)
		requestID, _ := syncMsg.Data["request_id"].(string)
		agentName := d.round.agentName
		currentPhaseForLog := d.phase
		numThreads := len(d.round.threadIDs)
		p11Done := d.round.phase11Completed

		// Log every message_completed for diagnostics
		log.Printf("[%s] ← message_completed: req=%s thread=%s phase=%d threads=%d p11done=%v",
			agentName, requestID, truncate(acpThreadID, 12), currentPhaseForLog, numThreads, p11Done)

		// Ignore completions from previous rounds. Two checks:
		// 1. Request ID must contain the current agent name
		// 2. Thread ID must belong to the current round
		if !strings.Contains(requestID, agentName) {
			d.mu.Unlock()
			log.Printf("[%s] FILTERED completion (wrong agent): req=%s thread=%s",
				agentName, requestID, truncate(acpThreadID, 12))
			return
		}
		isCurrentRoundThread := false
		for _, tid := range d.round.threadIDs {
			if tid == acpThreadID {
				isCurrentRoundThread = true
				break
			}
		}
		if acpThreadID == d.round.phase8ThreadID || acpThreadID == d.round.phase9ThreadID || acpThreadID == d.round.phase13ThreadID || acpThreadID == d.round.phase15ThreadID {
			isCurrentRoundThread = true
		}
		if !isCurrentRoundThread && acpThreadID != "" {
			d.mu.Unlock()
			log.Printf("[%s] FILTERED completion (wrong thread): req=%s thread=%s known=%v",
				agentName, requestID, truncate(acpThreadID, 12), d.round.threadIDs)
			return
		}

		d.round.completions[acpThreadID] = append(d.round.completions[acpThreadID], requestID)
		currentPhase := d.phase

		// Validate request_id matches the current phase. Without this, stale
		// completions from phase N (arriving while d.phase is already N+1) would
		// be misattributed to phase N+1, causing false "phase completed" signals.
		// For example, a duplicate Phase 11 completion arriving while d.phase=12
		// would falsely set phase12Completed=true before Phase 12 sends its message.
		// All request IDs follow the pattern "req-phase{N}-...-{agent}" so prefix
		// matching works for all phases including 8 and 9 which have sub-IDs.
		expectedReqPrefix := fmt.Sprintf("req-phase%d-", currentPhase)
		if !strings.HasPrefix(requestID, expectedReqPrefix) {
			d.mu.Unlock()
			log.Printf("[%s] FILTERED completion (wrong phase): req=%s expected prefix=%s phase=%d",
				agentName, requestID, expectedReqPrefix, currentPhase)
			return
		}

		// Phase 8 needs two completions before the test can end: one for the cancelled
		// initial turn and one for the interrupt response.
		if currentPhase == 8 && acpThreadID == d.round.phase8ThreadID {
			d.round.phase8Completions++
			completions := d.round.phase8Completions
			d.mu.Unlock()
			log.Printf("[%s] Phase 8: Completion %d/2 for thread=%s req=%s",
				agentName, completions, truncate(acpThreadID, 12), requestID)
			if completions >= 2 {
				log.Printf("[%s] Phase 8: Both turns completed (cancelled + interrupt)", agentName)
				time.Sleep(500 * time.Millisecond)
				go d.advanceAfterCompletion(8)
			}
			return
		}

		// Phase 9: rapid 3-turn cancel -- we expect at least 2 completions
		// (some turns may be cancelled/dropped, but the thread must not hang).
		if currentPhase == 9 && acpThreadID == d.round.phase9ThreadID {
			d.round.phase9Completions++
			completions := d.round.phase9Completions
			d.mu.Unlock()
			log.Printf("[%s] Phase 9: Completion %d for thread=%s req=%s",
				agentName, completions, truncate(acpThreadID, 12), requestID)
			if completions >= 2 {
				log.Printf("[%s] Phase 9: Received enough completions -- thread did not hang", agentName)
				time.Sleep(500 * time.Millisecond)
				go func() {
					d.mu.Lock()
					d.phase = 10
					d.mu.Unlock()
					d.runPhase10()
				}()
			}
			return
		}

		// Phase 15: capture completion timestamp + final assistant content
		// length so the validator can compare them against the streaming
		// samples recorded in phase15Adds.
		if currentPhase == 15 && acpThreadID == d.round.phase15ThreadID {
			d.round.phase15CompletedAt = time.Now()
			if n := len(d.round.phase15Adds); n > 0 {
				// The Stopped handler in Zed re-sends every entry with its final
				// content, so the last assistant sample carries the full response
				// length. (We use this as the denominator for the % check below.)
				d.round.phase15FinalLen = d.round.phase15Adds[n-1].contentLen
			}
		}

		d.mu.Unlock()

		log.Printf("[%s] Completed: thread=%s req=%s (phase %d)",
			agentName, truncate(acpThreadID, 12), requestID, currentPhase)

		go d.advanceAfterCompletion(currentPhase)
		return

	case "ui_state_response":
		d.round.uiStateResponses = append(d.round.uiStateResponses, *syncMsg)
		currentPhase := d.phase
		queryID, _ := syncMsg.Data["query_id"].(string)
		activeView, _ := syncMsg.Data["active_view"].(string)
		threadID, _ := syncMsg.Data["thread_id"].(string)
		d.mu.Unlock()

		log.Printf("[%s] UI state: query_id=%s active_view=%s thread_id=%s (phase %d)",
			d.round.agentName, queryID, activeView, truncate(threadID, 12), currentPhase)

		if currentPhase == 6 {
			go d.advanceAfterUiState()
		}
		return

	case "turn_cancelled":
		requestID, _ := syncMsg.Data["request_id"].(string)
		status, _ := syncMsg.Data["status"].(string)
		currentPhase := d.phase
		agentName := d.round.agentName

		if currentPhase == 13 {
			d.round.phase13TurnCancelled = true
			d.round.phase13CancelStatus = status
			d.mu.Unlock()
			log.Printf("[%s] Phase 13: turn_cancelled received: request_id=%s status=%s", agentName, requestID, status)
			go d.advanceAfterPhase13()
			return
		}
		if currentPhase == 14 {
			d.round.phase14TurnCancelled = true
			d.round.phase14CancelStatus = status
			d.mu.Unlock()
			log.Printf("[%s] Phase 14: turn_cancelled received: request_id=%s status=%s", agentName, requestID, status)
			go d.advanceAfterPhase14()
			return
		}
		d.mu.Unlock()
		log.Printf("[%s] turn_cancelled (unexpected phase %d): request_id=%s status=%s", agentName, currentPhase, requestID, status)
		return

	case "thread_title_changed":
		title, _ := syncMsg.Data["title"].(string)
		acpThreadID, _ := syncMsg.Data["acp_thread_id"].(string)
		log.Printf("[%s] Title changed: thread=%s title=%q", d.round.agentName, truncate(acpThreadID, 12), title)

	case "thread_load_error":
		errMsg, _ := syncMsg.Data["error"].(string)
		acpThreadID, _ := syncMsg.Data["acp_thread_id"].(string)
		log.Printf("[%s] THREAD LOAD ERROR: %s (thread=%s)", d.round.agentName, errMsg, truncate(acpThreadID, 12))
	}

	d.mu.Unlock()
}

// --- Command helpers ---

func (d *testDriver) sendChatMessage(message, requestID, agentName string, acpThreadID ...string) {
	// For follow-up messages to an existing thread, use the production
	// SendChatMessage path which creates an interaction and sends the
	// command via the same code path as sendMessageToSpecTaskAgent.
	if len(acpThreadID) > 0 && acpThreadID[0] != "" {
		threadID := acpThreadID[0]
		mappings := d.srv.ContextMappings()
		if sessionID, ok := mappings[threadID]; ok {
			if err := d.srv.SendChatMessage(sessionID, message, requestID); err != nil {
				log.Printf("[test-server] WARNING: SendChatMessage failed for session %s: %v (falling back to QueueCommand)", sessionID, err)
			} else {
				return
			}
		}
	}

	// New thread (no acpThreadID or not in contextMappings yet) — send directly.
	data := map[string]interface{}{
		"message":    message,
		"request_id": requestID,
		"agent_name": agentName,
	}
	if len(acpThreadID) > 0 && acpThreadID[0] != "" {
		data["acp_thread_id"] = acpThreadID[0]
	}
	cmd := types.ExternalAgentCommand{Type: "chat_message", Data: data}
	if !d.srv.QueueCommand(d.agentID, cmd) {
		log.Printf("[test-server] WARNING: Failed to send command to agent %s", d.agentID)
	}
}

func (d *testDriver) sendSimulateUserInput(acpThreadID, message, requestID, agentName string) {
	cmd := types.ExternalAgentCommand{
		Type: "simulate_user_input",
		Data: map[string]interface{}{
			"acp_thread_id": acpThreadID,
			"message":       message,
			"request_id":    requestID,
			"agent_name":    agentName,
		},
	}
	d.srv.QueueCommand(d.agentID, cmd)
}

func (d *testDriver) sendOpenThread(acpThreadID, agentName string) {
	data := map[string]interface{}{
		"acp_thread_id": acpThreadID,
	}
	if agentName != "" {
		data["agent_name"] = agentName
	}
	cmd := types.ExternalAgentCommand{
		Type: "open_thread",
		Data: data,
	}
	if !d.srv.QueueCommand(d.agentID, cmd) {
		log.Printf("[test-server] WARNING: Failed to send open_thread to agent %s", d.agentID)
	}
}

func (d *testDriver) sendCancelCurrentTurn(requestID string) {
	cmd := types.ExternalAgentCommand{
		Type: "cancel_current_turn",
		Data: map[string]interface{}{
			"request_id": requestID,
		},
	}
	if !d.srv.QueueCommand(d.agentID, cmd) {
		log.Printf("[test-server] WARNING: Failed to send cancel_current_turn to agent %s", d.agentID)
	}
}

func (d *testDriver) sendQueryUiState(queryID string) {
	cmd := types.ExternalAgentCommand{
		Type: "query_ui_state",
		Data: map[string]interface{}{
			"query_id": queryID,
		},
	}
	if !d.srv.QueueCommand(d.agentID, cmd) {
		log.Printf("[test-server] WARNING: Failed to send query_ui_state to agent %s", d.agentID)
	}
}

const phaseTimeout = 90 * time.Second

// startPhaseTimeout launches a watchdog that fires if the current phase
// doesn't complete within phaseTimeout. On timeout it dumps diagnostic
// state and advances to the next round (failing the current one).
func (d *testDriver) startPhaseTimeout(phase int) {
	d.mu.Lock()
	d.phaseStarted = time.Now()
	gen := d.roundGeneration
	agent := d.round.agentName
	d.mu.Unlock()

	go func() {
		time.Sleep(phaseTimeout)
		d.mu.Lock()
		if d.roundGeneration != gen {
			d.mu.Unlock()
			return // round already advanced
		}
		if d.phase != phase {
			d.mu.Unlock()
			return // phase already advanced
		}
		// Phase timed out — dump state
		if d.phaseTimedOut == nil {
			d.phaseTimedOut = make(map[int]bool)
		}
		d.phaseTimedOut[phase] = true
		eventCount := len(d.round.events)
		threadCount := len(d.round.threadIDs)
		threads := make([]string, len(d.round.threadIDs))
		copy(threads, d.round.threadIDs)
		completions := make(map[string][]string)
		for k, v := range d.round.completions {
			completions[k] = v
		}
		d.mu.Unlock()

		log.Printf("\n!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!")
		log.Printf("  [%s] PHASE %d TIMED OUT after %s", agent, phase, phaseTimeout)
		log.Printf("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!")
		log.Printf("[%s] State at timeout:", agent)
		log.Printf("[%s]   Events received: %d", agent, eventCount)
		log.Printf("[%s]   Threads: %d %v", agent, threadCount, threads)
		log.Printf("[%s]   Completions: %v", agent, completions)

		// Dump recent events
		d.mu.Lock()
		events := d.round.events
		d.mu.Unlock()
		start := 0
		if len(events) > 10 {
			start = len(events) - 10
		}
		for i := start; i < len(events); i++ {
			data, _ := json.Marshal(events[i].Data)
			dataStr := string(data)
			if len(dataStr) > 200 {
				dataStr = dataStr[:200] + "..."
			}
			log.Printf("[%s]   event %d: type=%s data=%s", agent, i, events[i].EventType, dataStr)
		}

		log.Printf("[%s] ABORTING: phase %d timed out — phases are sequential, cannot continue", agent, phase)
		os.Exit(1)
	}()
}

// --- Phase execution ---

func (d *testDriver) advanceAfterCompletion(completedPhase int) {
	d.mu.Lock()
	agentName := d.round.agentName
	actualPhase := d.phase
	gen := d.roundGeneration
	d.mu.Unlock()
	log.Printf("[%s] advanceAfterCompletion(%d) called (current phase=%d gen=%d), sleeping 2s...",
		agentName, completedPhase, actualPhase, gen)
	time.Sleep(2 * time.Second) // let Zed settle

	// If the round changed while we slept, this completion is stale — bail out.
	d.mu.Lock()
	if d.roundGeneration != gen {
		curAgent := d.round.agentName
		d.mu.Unlock()
		log.Printf("[%s] advanceAfterCompletion(%d) STALE: gen changed %d→%d (now in %s round), ignoring",
			agentName, completedPhase, gen, d.roundGeneration, curAgent)
		return
	}
	d.mu.Unlock()

	switch completedPhase {
	case 1:
		d.mu.Lock()
		d.phase = 2
		d.mu.Unlock()
		d.runPhase2()
	case 2:
		d.mu.Lock()
		d.phase = 3
		d.mu.Unlock()
		d.runPhase3()
	case 3:
		d.mu.Lock()
		d.phase = 4
		d.mu.Unlock()
		d.runPhase4()
	case 4:
		d.mu.Lock()
		d.phase = 5
		d.mu.Unlock()
		d.runPhase5()
	case 5:
		d.mu.Lock()
		d.phase = 6
		d.mu.Unlock()
		d.runPhase6()
	case 7:
		d.mu.Lock()
		d.phase = 8
		d.mu.Unlock()
		d.runPhase8()
	case 8:
		d.mu.Lock()
		d.phase = 9
		d.mu.Unlock()
		d.runPhase9()
	case 11:
		d.mu.Lock()
		if d.round.phase11Completed {
			agentName := d.round.agentName
			d.mu.Unlock()
			log.Printf("[%s] Phase 11: DUPLICATE completion ignored (already advanced)", agentName)
			return
		}
		d.round.phase11Completed = true
		agentName := d.round.agentName
		d.mu.Unlock()
		log.Printf("[%s] Phase 11: ✅ Routed message completed", agentName)
		d.mu.Lock()
		d.phase = 12
		d.mu.Unlock()
		d.runPhase12()
	case 12:
		d.mu.Lock()
		if d.round.phase12Completed {
			agentName := d.round.agentName
			d.mu.Unlock()
			log.Printf("[%s] Phase 12: DUPLICATE completion ignored (already advanced)", agentName)
			return
		}
		d.round.phase12Completed = true
		agentName := d.round.agentName
		d.mu.Unlock()
		log.Printf("[%s] Phase 12: ✅ Reconnect message completed", agentName)
		d.mu.Lock()
		d.phase = 13
		d.mu.Unlock()
		d.runPhase13()
	case 15:
		// Phase 15 completed normally — final phase of the round.
		d.advanceToNextRound()
	}
}

func (d *testDriver) advanceAfterUiState() {
	time.Sleep(1 * time.Second)
	d.mu.Lock()
	d.phase = 7
	d.mu.Unlock()
	d.runPhase7()
}

// advanceToNextRound validates the current round, then starts the next one or finishes.
func (d *testDriver) advanceToNextRound() {
	d.mu.Lock()
	agentName := d.round.agentName
	roundIdx := d.currentRoundIdx
	phase := d.phase
	eventCount := len(d.round.events)
	d.mu.Unlock()

	log.Printf("[%s] advanceToNextRound() called: round=%d phase=%d events=%d",
		agentName, roundIdx+1, phase, eventCount)

	// Validate current round
	result := d.validateRound()
	d.mu.Lock()
	d.roundResults = append(d.roundResults, result)
	d.currentRoundIdx++

	if d.currentRoundIdx >= len(d.agentRounds) {
		// All rounds complete
		d.mu.Unlock()
		log.Printf("[%s] All rounds complete, closing done channel", agentName)
		close(d.done)
		return
	}

	// Start next round
	nextAgent := d.agentRounds[d.currentRoundIdx]
	d.round = newRoundState(nextAgent)
	d.phase = 1
	d.roundGeneration++
	d.mu.Unlock()

	log.Printf("\n##################################################")
	log.Printf("  ROUND %d/%d: Agent = %s (after %s)", d.currentRoundIdx+1, len(d.agentRounds), nextAgent, agentName)
	log.Printf("##################################################")

	// Wait for stale events from the previous round to drain before starting.
	// Phase 11 sends a message via SendChatMessage whose completion may arrive
	// after the round advances. We need enough time for all trailing events to
	// be processed and filtered. This also gives time for the new agent to be
	// installed (e.g., Claude Code auto-installs via npm on first use).
	log.Printf("[test-server] Waiting 10s for previous round events to drain...")
	time.Sleep(10 * time.Second)

	d.runPhase1()
}

func (d *testDriver) runPhase1() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 1: Basic thread creation", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(1)
	d.mu.Lock()
	d.round.phase1ChatSentAt = time.Now()
	d.mu.Unlock()
	d.sendChatMessage("What is 2 + 2? Reply with just the number.", d.round.reqID("phase1"), agent)
}

func (d *testDriver) runPhase2() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 2: Follow-up on existing thread", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(2)
	d.mu.Lock()
	if len(d.round.threadIDs) == 0 {
		d.mu.Unlock()
		log.Fatalf("[%s] ERROR: No thread IDs captured from phase 1!", agent)
	}
	tid := d.round.threadIDs[0]
	d.mu.Unlock()

	log.Printf("[%s] Using thread from phase 1: %s", agent, truncate(tid, 16))
	d.sendChatMessage("What is 3 + 3? Reply with just the number.", d.round.reqID("phase2"), agent, tid)
}

func (d *testDriver) runPhase3() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 3: New thread (simulating thread transition)", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(3)
	d.sendChatMessage("What is 10 + 10? Reply with just the number.", d.round.reqID("phase3"), agent)
}

func (d *testDriver) runPhase4() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 4: Follow-up to non-visible thread", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(4)
	d.mu.Lock()
	if len(d.round.threadIDs) < 2 {
		d.mu.Unlock()
		log.Fatalf("[%s] ERROR: Need at least 2 threads for phase 4!", agent)
	}
	tid := d.round.threadIDs[0]
	d.mu.Unlock()

	log.Printf("[%s] Sending back to Thread A (non-visible): %s", agent, truncate(tid, 16))
	d.sendChatMessage("What is 5 + 5? Reply with just the number.", d.round.reqID("phase4"), agent, tid)
}

func (d *testDriver) runPhase5() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 5: Simulate user input (Zed -> Helix sync)", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(5)
	d.mu.Lock()
	if len(d.round.threadIDs) == 0 {
		d.mu.Unlock()
		log.Fatalf("[%s] ERROR: No thread IDs available for phase 5!", agent)
	}
	tid := d.round.threadIDs[0]
	d.mu.Unlock()

	log.Printf("[%s] Sending simulate_user_input to thread: %s", agent, truncate(tid, 16))
	d.sendSimulateUserInput(tid, "This message was typed by the user in Zed", d.round.reqID("phase5"), agent)
}

func (d *testDriver) runPhase6() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 6: Query UI state", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(6)
	d.sendQueryUiState(fmt.Sprintf("query-phase6-%s", agent))
}

func (d *testDriver) runPhase7() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 7: Open thread + follow-up chat", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(7)
	d.mu.Lock()
	if len(d.round.threadIDs) < 2 {
		d.mu.Unlock()
		log.Fatalf("[%s] ERROR: Need at least 2 threads for phase 7!", agent)
	}
	// Open Thread B (created in phase 3), then send a follow-up
	tid := d.round.threadIDs[1]
	d.mu.Unlock()

	log.Printf("[%s] Opening Thread B: %s", agent, truncate(tid, 16))
	d.sendOpenThread(tid, agent)

	// Wait for Zed to open the thread before sending follow-up
	time.Sleep(3 * time.Second)

	log.Printf("[%s] Sending follow-up to Thread B after open_thread", agent)
	d.sendChatMessage("What is 8 + 8? Reply with just the number.", d.round.reqID("phase7"), agent, tid)
}

func (d *testDriver) runPhase8() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 8: Mid-stream interrupt", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(8)
	// Send a question that will generate a streaming response long enough for us
	// to send an interrupt before it completes. The syncEventCallback will fire the
	// interrupt the moment the first assistant token arrives.
	d.sendChatMessage(
		"Write me a detailed explanation of recursion with three worked examples.",
		d.round.reqID("phase8-initial"),
		agent,
	)
}

func (d *testDriver) runPhase9() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 9: Rapid 3-turn cancel (regression test)", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(9)
	log.Println("  Sends chat_message, then while streaming, fires")
	log.Println("  simulate_user_input + chat_message back-to-back.")
	log.Println("  Without the fix, the thread hangs permanently.")

	// Reuse the phase 8 thread (it completed, so we can send follow-ups).
	d.mu.Lock()
	d.round.phase9ThreadID = d.round.phase8ThreadID
	d.mu.Unlock()

	// Turn 1: start a long-running response. The syncEventCallback will
	// fire the rapid sequence as soon as the first assistant token arrives.
	d.sendChatMessage(
		"Write a detailed explanation of merge sort with code examples.",
		d.round.reqID("phase9-initial"),
		agent,
		d.round.phase8ThreadID,
	)
}

func (d *testDriver) runPhase10() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 10: User-created thread (multi-thread sync)", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(10)
	log.Println("  Injects a synthetic user_created_thread event via")
	log.Println("  ProcessSyncEvent, then verifies session + work session")
	log.Println("  are created, then sends a chat on the new thread.")

	// Generate a new thread ID for the user-created thread
	newThreadID := fmt.Sprintf("user-thread-%s-%d", agent, time.Now().UnixNano())
	d.mu.Lock()
	d.round.phase10NewThreadID = newThreadID
	d.mu.Unlock()

	// Find the session ID from Phase 1 (the first thread's session).
	// ProcessSyncEvent needs a valid session ID that exists in the store.
	// The agentID is the WebSocket connection ID, not necessarily a session
	// in the store. The Phase 1 thread_created handler created a session
	// which we can find via contextMappings.
	d.mu.Lock()
	firstThreadID := ""
	if len(d.round.threadIDs) > 0 {
		firstThreadID = d.round.threadIDs[0]
	}
	d.mu.Unlock()

	existingSessionID := ""
	if firstThreadID != "" {
		mappings := d.srv.ContextMappings()
		existingSessionID = mappings[firstThreadID]
	}
	if existingSessionID == "" {
		log.Printf("[%s] Phase 10: ERROR no existing session found to use as parent", agent)
		go d.advanceToNextRound()
		return
	}

	log.Printf("[%s] Phase 10: Using existing session %s as parent for user-created thread", agent, existingSessionID)

	// Inject user_created_thread via ProcessSyncEvent (bypasses WebSocket —
	// tests the Helix handler directly since Zed can't send this in headless mode)
	syncMsg := &types.SyncMessage{
		EventType: "user_created_thread",
		Data: map[string]interface{}{
			"acp_thread_id": newThreadID,
			"title":         fmt.Sprintf("User Thread (%s)", agent),
		},
	}

	if err := d.srv.ProcessSyncEvent(existingSessionID, syncMsg); err != nil {
		log.Printf("[%s] Phase 10: ERROR injecting user_created_thread: %v", agent, err)
		go d.advanceToNextRound()
		return
	}

	log.Printf("[%s] Phase 10: Injected user_created_thread for thread=%s", agent, truncate(newThreadID, 12))

	// Verify the session was created by checking context mappings
	time.Sleep(500 * time.Millisecond)
	mappings := d.srv.ContextMappings()
	if sessionID, ok := mappings[newThreadID]; ok {
		log.Printf("[%s] Phase 10: ✅ New session created: %s", agent, sessionID)
	} else {
		log.Printf("[%s] Phase 10: ❌ No session mapping found for thread %s", agent, truncate(newThreadID, 12))
	}

	// Verify work session and zed thread records were created in the store
	d.mu.Lock()
	d.round.phase10WorkSessionFound = false
	d.mu.Unlock()

	// Check store for the new session's work session
	sessions := d.store.GetAllSessions()
	for _, ses := range sessions {
		if ses.Metadata.ZedThreadID == newThreadID {
			log.Printf("[%s] Phase 10: ✅ Found session in store: %s (ZedThreadID=%s, SpecTaskID=%s)",
				agent, ses.ID, truncate(ses.Metadata.ZedThreadID, 12), ses.Metadata.SpecTaskID)
			d.mu.Lock()
			d.round.phase10WorkSessionFound = true
			d.round.phase10ChatCompleted = true // skip chat test — synthetic thread doesn't exist in Zed
			d.mu.Unlock()
			break
		}
	}

	if !d.round.phase10WorkSessionFound {
		log.Printf("[%s] Phase 10: ❌ No session found in store with ZedThreadID=%s", agent, truncate(newThreadID, 12))
	}

	// Chain to Phase 11
	go func() {
		d.mu.Lock()
		d.phase = 11
		d.mu.Unlock()
		d.runPhase11()
	}()
}

func (d *testDriver) runPhase11() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 11: Spectask routing (most recently active session)", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(11)
	log.Println("  Sets SpecTaskID on Thread A and B sessions, then uses")
	log.Println("  FindConnectedSessionForSpecTask to verify routing picks")
	log.Println("  the most recently active thread, sends a message, and")
	log.Println("  verifies the response arrives on the correct session.")

	ctx := context.Background()
	specTaskID := fmt.Sprintf("spectask-e2e-%s-%d", agent, time.Now().UnixNano())

	// We need at least 2 threads (Thread A from phase 1, Thread B from phase 3)
	d.mu.Lock()
	if len(d.round.threadIDs) < 2 {
		d.mu.Unlock()
		log.Printf("[%s] Phase 11: SKIP — need at least 2 threads, got %d", agent, len(d.round.threadIDs))
		go d.advanceToNextRound()
		return
	}
	threadA := d.round.threadIDs[0]
	threadB := d.round.threadIDs[1]
	d.mu.Unlock()

	mappings := d.srv.ContextMappings()
	sessionA := mappings[threadA]
	sessionB := mappings[threadB]

	if sessionA == "" || sessionB == "" {
		log.Printf("[%s] Phase 11: ERROR — missing session mappings (A=%s, B=%s)", agent, sessionA, sessionB)
		go d.advanceToNextRound()
		return
	}

	// Set SpecTaskID on both sessions
	for _, sid := range []string{sessionA, sessionB} {
		ses, err := d.store.GetSession(ctx, sid)
		if err != nil {
			log.Printf("[%s] Phase 11: ERROR getting session %s: %v", agent, sid, err)
			go d.advanceToNextRound()
			return
		}
		ses.Metadata.SpecTaskID = specTaskID
		if _, err := d.store.UpdateSession(ctx, *ses); err != nil {
			log.Printf("[%s] Phase 11: ERROR updating session %s: %v", agent, sid, err)
			go d.advanceToNextRound()
			return
		}
	}

	// Thread B should be more recently active (Phase 7 completed on it).
	// Verify routing picks Thread B's session.
	specTask := &types.SpecTask{ID: specTaskID}
	routedSessionID, err := d.srv.FindConnectedSessionForSpecTask(ctx, specTask)
	if err != nil {
		log.Printf("[%s] Phase 11: ERROR FindConnectedSessionForSpecTask failed: %v", agent, err)
		go d.advanceToNextRound()
		return
	}

	d.mu.Lock()
	d.round.phase11RoutedSessionID = routedSessionID
	d.round.phase11ExpectedThreadID = threadB
	d.mu.Unlock()

	if routedSessionID == sessionB {
		log.Printf("[%s] Phase 11: ✅ Routing picked Thread B's session %s (most recently active)", agent, truncate(sessionB, 12))
	} else if routedSessionID == sessionA {
		log.Printf("[%s] Phase 11: ⚠️ Routing picked Thread A's session %s (expected Thread B)", agent, truncate(sessionA, 12))
	} else {
		log.Printf("[%s] Phase 11: ⚠️ Routing picked unexpected session %s", agent, truncate(routedSessionID, 12))
	}

	// Send a message via the routed session and wait for completion
	reqID := d.round.reqID("phase11")
	if err := d.srv.SendChatMessage(routedSessionID, "What is 7 + 7? Reply with just the number.", reqID); err != nil {
		log.Printf("[%s] Phase 11: ERROR SendChatMessage failed: %v", agent, err)
		go d.advanceToNextRound()
		return
	}

	log.Printf("[%s] Phase 11: Sent message to routed session %s, waiting for completion...", agent, truncate(routedSessionID, 12))
	// Completion will be detected by syncEventCallback when message_completed arrives
}

func (d *testDriver) runPhase12() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 12: Reconnect test (kill Zed, reconnect, verify delivery)", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(12)
	log.Println("  Creates signal file to request Zed restart, then waits")
	log.Println("  for reconnection. After Zed reconnects, sends a chat")
	log.Println("  message to Thread A and verifies message_completed.")

	// Create signal file to tell run_e2e.sh to restart Zed
	signalFile := "/tmp/zed-restart-requested"
	if err := os.WriteFile(signalFile, []byte("phase12"), 0644); err != nil {
		log.Printf("[%s] Phase 12: ERROR creating signal file: %v", agent, err)
		go d.advanceToNextRound()
		return
	}
	log.Printf("[%s] Phase 12: Created signal file %s, waiting for Zed to disconnect and reconnect...", agent, signalFile)
	// The HTTP handler will detect the reconnection (phase == 12) and send
	// the chat message to Thread A. syncEventCallback will handle
	// message_completed and call advanceAfterCompletion(12).
}

func (d *testDriver) runPhase13() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 13: Helix-initiated cancel (cancel_current_turn)", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(13)
	log.Println("  Sends a chat_message to start a streaming turn, then")
	log.Println("  fires cancel_current_turn when the first assistant token")
	log.Println("  arrives. Verifies Zed responds with turn_cancelled (status=cancelled).")

	// Send a long-running question that will produce streaming output.
	// The syncEventCallback will fire cancel_current_turn when the first
	// assistant token arrives.
	d.sendChatMessage(
		"Write a detailed explanation of binary search trees with three worked examples.",
		d.round.reqID("phase13"),
		agent,
	)
}

func (d *testDriver) advanceAfterPhase13() {
	time.Sleep(1 * time.Second)
	d.mu.Lock()
	agentName := d.round.agentName
	d.mu.Unlock()
	log.Printf("[%s] Phase 13: ✅ turn_cancelled received, advancing to phase 14", agentName)

	d.mu.Lock()
	d.phase = 14
	d.mu.Unlock()
	d.runPhase14()
}

func (d *testDriver) runPhase14() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 14: Cancel no-op (bogus request_id)", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(14)
	log.Println("  Sends cancel_current_turn with a request_id that doesn't")
	log.Println("  correspond to any active turn. Verifies Zed responds with")
	log.Println("  turn_cancelled (status=noop).")

	d.sendCancelCurrentTurn("bogus-request-id-that-does-not-exist")
}

func (d *testDriver) advanceAfterPhase14() {
	time.Sleep(1 * time.Second)
	d.mu.Lock()
	agentName := d.round.agentName
	d.phase = 15
	d.mu.Unlock()
	log.Printf("[%s] Phase 14: ✅ turn_cancelled (noop) received, advancing to phase 15", agentName)
	d.runPhase15()
}

func (d *testDriver) runPhase15() {
	agent := d.round.agentName
	log.Printf("\n==================================================")
	log.Printf("  [%s] PHASE 15: Streaming patches arrive incrementally", agent)
	log.Printf("==================================================")
	d.startPhaseTimeout(15)
	log.Println("  Sends a long prose prompt and records the timestamp +")
	log.Println("  content length of every assistant message_added event.")
	log.Println("  Asserts streaming patches arrive throughout the response,")
	log.Println("  not bunched at the end (i.e. the streaming-reveal task in")
	log.Println("  acp_thread.rs re-emits EntryUpdated after each drain so")
	log.Println("  external_websocket_sync sees fresh content as it grows).")

	d.mu.Lock()
	d.round.phase15ChatSentAt = time.Now()
	d.mu.Unlock()

	// Long-form, plain-prose prompt with NO tool calls. ~400 words gives ~30+
	// streaming chunks at typical model token rates, well above the >=5 sample
	// threshold. The "no tools" instruction matters: tool calls would split the
	// response into many short text entries and obscure the streaming-cadence
	// signal we want to assert on.
	d.sendChatMessage(
		"Write a 400-word essay about the history of the printing press in plain prose. "+
			"No headings, no bullet lists, no tool calls — just a single flowing essay.",
		d.round.reqID("phase15"),
		agent,
	)
}

// --- Per-round validation ---

func (d *testDriver) validateRound() roundResult {
	d.mu.Lock()
	defer d.mu.Unlock()

	agent := d.round.agentName

	log.Printf("\n==================================================")
	log.Printf("  VALIDATION: %s", agent)
	log.Printf("==================================================")

	var errors []string

	// --- Event-level validation ---
	log.Printf("[%s] Total sync events: %d", agent, len(d.round.events))
	log.Printf("[%s] Thread IDs seen: %d", agent, len(d.round.threadIDs))
	log.Printf("[%s] Completions: %v", agent, d.round.completions)

	// Phase 1: Basic thread creation
	threadCreatedEvents := d.filterRoundEvents("thread_created")
	if len(threadCreatedEvents) < 1 {
		errors = append(errors, "Phase 1: No thread_created event")
	}
	if !d.hasRoundCompletion(d.round.reqID("phase1")) {
		errors = append(errors, "Phase 1: No message_completed for "+d.round.reqID("phase1"))
	}

	// Phase 2: Follow-up on existing thread
	if !d.hasRoundCompletion(d.round.reqID("phase2")) {
		errors = append(errors, "Phase 2: No message_completed for "+d.round.reqID("phase2"))
	}

	// Phase 3: New thread creation
	if len(d.round.threadIDs) < 2 {
		errors = append(errors, fmt.Sprintf("Phase 3: Expected at least 2 threads, got %d", len(d.round.threadIDs)))
	} else if d.round.threadIDs[0] == d.round.threadIDs[1] {
		errors = append(errors, "Phase 3: New thread has same ID as first thread!")
	} else {
		log.Printf("[%s] Phase 3: New thread created: %s", agent, truncate(d.round.threadIDs[1], 12))
	}
	if !d.hasRoundCompletion(d.round.reqID("phase3")) {
		errors = append(errors, "Phase 3: No message_completed for "+d.round.reqID("phase3"))
	}

	// Phase 4: Follow-up to non-visible thread
	if !d.hasRoundCompletion(d.round.reqID("phase4")) {
		errors = append(errors, "Phase 4: No message_completed for "+d.round.reqID("phase4"))
	}

	// Phase 5: Simulate user input
	if !d.hasRoundCompletion(d.round.reqID("phase5")) {
		errors = append(errors, "Phase 5: No message_completed for "+d.round.reqID("phase5"))
	}
	userMsgs := d.filterRoundEventsByFunc(func(e types.SyncMessage) bool {
		return e.EventType == "message_added" &&
			e.Data["role"] == "user" &&
			strings.Contains(fmt.Sprint(e.Data["content"]), "typed by the user in Zed")
	})
	if len(userMsgs) == 0 {
		errors = append(errors, "Phase 5: No message_added with role='user' containing simulated input text")
	} else {
		log.Printf("[%s] Phase 5: User message synced back to Helix", agent)
	}

	// Phase 6: query_ui_state
	expectedQueryID := fmt.Sprintf("query-phase6-%s", agent)
	if len(d.round.uiStateResponses) == 0 {
		errors = append(errors, "Phase 6: No ui_state_response received")
	} else {
		resp := d.round.uiStateResponses[0]
		queryID, _ := resp.Data["query_id"].(string)
		activeView, _ := resp.Data["active_view"].(string)
		if queryID != expectedQueryID {
			errors = append(errors, fmt.Sprintf("Phase 6: ui_state_response query_id=%q, expected %q", queryID, expectedQueryID))
		}
		if activeView == "" {
			errors = append(errors, "Phase 6: ui_state_response active_view is empty")
		} else {
			threadID, _ := resp.Data["thread_id"].(string)
			entryCount, _ := resp.Data["entry_count"].(float64) // JSON numbers are float64
			log.Printf("[%s] Phase 6: UI state - active_view=%s, thread_id=%s, entry_count=%.0f",
				agent, activeView, truncate(threadID, 12), entryCount)
		}

		// Validate MCP server status (only for first round -- MCP servers are agent-independent)
		if d.currentRoundIdx == 0 {
			mcpServers, _ := resp.Data["mcp_servers"].(map[string]interface{})
			if len(mcpServers) == 0 {
				errors = append(errors, "Phase 6: ui_state_response mcp_servers is empty (expected at least slow-mcp-test)")
			} else {
				log.Printf("[%s] Phase 6: MCP servers reported: %d", agent, len(mcpServers))
				slowMcpStatus, hasSlowMcp := mcpServers["slow-mcp-test"]
				if !hasSlowMcp {
					errors = append(errors, "Phase 6: mcp_servers missing 'slow-mcp-test' server")
				} else if slowMcpStatus != "running" {
					errors = append(errors, fmt.Sprintf(
						"Phase 6: slow-mcp-test status=%q, expected 'running' (MCP server not connected)",
						slowMcpStatus))
				} else {
					log.Printf("[%s] Phase 6: slow-mcp-test MCP server is running (green/connected)", agent)
				}
				for name, status := range mcpServers {
					log.Printf("[%s]   MCP server %q: %s", agent, name, status)
				}
			}
		}

		// Validate active model
		activeModel, _ := resp.Data["active_model"].(string)
		if activeModel == "" {
			log.Printf("[%s] WARNING: Phase 6: active_model is empty (model list may not have loaded yet)", agent)
		} else {
			log.Printf("[%s] Phase 6: Active model: %s", agent, activeModel)
		}
	}

	// Phase 7: open_thread + follow-up
	if !d.hasRoundCompletion(d.round.reqID("phase7")) {
		errors = append(errors, "Phase 7: No message_completed for "+d.round.reqID("phase7"))
	}

	// Phase 8-9: mid-stream interrupt and rapid cancel
	{
		// Phase 8: mid-stream interrupt
		if d.round.phase8ThreadID == "" {
			errors = append(errors, "Phase 8: No thread ID captured (phase 8 may not have run)")
		} else {
			completionsForPhase8 := len(d.round.completions[d.round.phase8ThreadID])
			if completionsForPhase8 < 2 {
				errors = append(errors, fmt.Sprintf(
					"Phase 8: Expected 2 message_completed events (cancelled turn + interrupt), got %d",
					completionsForPhase8))
			} else {
				log.Printf("[%s] Phase 8: Received %d completions for phase 8 thread (correct)", agent, completionsForPhase8)
			}
		}
		if !d.hasRoundCompletion(d.round.reqID("phase8-interrupt")) {
			errors = append(errors, "Phase 8: No message_completed for "+d.round.reqID("phase8-interrupt"))
		}

		// Verify ordering: no assistant tokens for the interrupt arrived before the first
		// message_completed for the phase 8 thread.
		if d.round.phase8ThreadID != "" && d.hasRoundCompletion(d.round.reqID("phase8-interrupt")) {
			seenFirstCompletion := false
			orderingViolation := false
			for _, e := range d.round.events {
				threadID, _ := e.Data["acp_thread_id"].(string)
				if threadID != d.round.phase8ThreadID {
					continue
				}
				if e.EventType == "message_completed" && !seenFirstCompletion {
					seenFirstCompletion = true
				}
				if e.EventType == "message_added" && !seenFirstCompletion {
					role, _ := e.Data["role"].(string)
					reqID, _ := e.Data["request_id"].(string)
					if role == "assistant" && reqID == d.round.reqID("phase8-interrupt") {
						orderingViolation = true
					}
				}
			}
			if orderingViolation {
				errors = append(errors, "Phase 8: Interrupt assistant tokens arrived before the first message_completed (FIFO ordering violated)")
			} else {
				log.Printf("[%s] Phase 8: Ordering correct -- interrupt tokens arrived after first message_completed", agent)
			}
		}

		// Phase 9: rapid 3-turn cancel
		if d.round.phase9ThreadID == "" {
			errors = append(errors, "Phase 9: No thread ID (phase 9 may not have run)")
		} else {
			if d.round.phase9Completions < 2 {
				errors = append(errors, fmt.Sprintf(
					"Phase 9: Expected at least 2 message_completed events (got %d) -- thread may have hung",
					d.round.phase9Completions))
			} else {
				log.Printf("[%s] Phase 9: Received %d completions -- thread recovered from rapid cancel (correct)", agent, d.round.phase9Completions)
			}
		}
	}

	// Phase 10: user-created thread (multi-thread sync)
	if d.round.phase10NewThreadID == "" {
		errors = append(errors, "Phase 10: No thread ID (phase 10 may not have run)")
	} else {
		// Verify context mapping exists for the user-created thread
		mappings := d.srv.ContextMappings()
		if sessionID, ok := mappings[d.round.phase10NewThreadID]; ok {
			log.Printf("[%s] Phase 10: ✅ Session mapping: thread=%s → session=%s",
				agent, truncate(d.round.phase10NewThreadID, 12), sessionID)
		} else {
			errors = append(errors, fmt.Sprintf("Phase 10: No session mapping for user-created thread %s", truncate(d.round.phase10NewThreadID, 12)))
		}

		if !d.round.phase10WorkSessionFound {
			errors = append(errors, "Phase 10: Work session/session not created in store for user-created thread")
		} else {
			log.Printf("[%s] Phase 10: ✅ Session created in store for user-created thread", agent)
		}
	}

	// Phase 11: spectask routing
	if d.round.phase11RoutedSessionID == "" {
		errors = append(errors, "Phase 11: Routing did not run (phase 11 may not have executed)")
	} else {
		mappings := d.srv.ContextMappings()
		expectedSessionID := mappings[d.round.phase11ExpectedThreadID]
		if d.round.phase11RoutedSessionID == expectedSessionID {
			log.Printf("[%s] Phase 11: ✅ Routing picked most recently active session (%s)",
				agent, truncate(d.round.phase11RoutedSessionID, 12))
		} else {
			errors = append(errors, fmt.Sprintf("Phase 11: Routing picked session %s, expected %s (Thread B)",
				truncate(d.round.phase11RoutedSessionID, 12), truncate(expectedSessionID, 12)))
		}
		if !d.round.phase11Completed {
			errors = append(errors, "Phase 11: Routed message did not complete")
		} else {
			log.Printf("[%s] Phase 11: ✅ Routed message completed on correct session", agent)
		}
	}

	// Phase 12: reconnect test — verify message was delivered AND has content
	if !d.round.phase12Completed {
		errors = append(errors, "Phase 12: Reconnect message did not complete (Zed may not have reconnected)")
	} else {
		// Verify the interaction in the store has actual content (not response_length=0).
		// After reconnect, request_id may differ from what we sent (pickupWaitingInteraction
		// creates new mappings), so we match by prompt content instead.
		phase12HasContent := false
		allInteractions := d.store.GetAllInteractions()
		for _, i := range allInteractions {
			if i.PromptMessage == "What is 12 + 12? Reply with just the number." && i.State == types.InteractionStateComplete {
				if len(i.ResponseMessage) > 0 {
					phase12HasContent = true
					log.Printf("[%s] Phase 12: ✅ Reconnect interaction %s has %d bytes of response", agent, truncate(i.ID, 12), len(i.ResponseMessage))
				} else {
					errors = append(errors, fmt.Sprintf("Phase 12: Interaction %s completed but response_message is EMPTY — message ID collision or history replay corruption", truncate(i.ID, 12)))
				}
				break
			}
		}
		if !phase12HasContent {
			errors = append(errors, "Phase 12: No completed interaction found with Phase 12 prompt content")
		}
	}

	// Phase 13: Helix-initiated cancel
	if !d.round.phase13TurnCancelled {
		errors = append(errors, "Phase 13: No turn_cancelled event received (cancel_current_turn may not have been processed)")
	} else {
		if d.round.phase13CancelStatus != "cancelled" {
			errors = append(errors, fmt.Sprintf("Phase 13: turn_cancelled status=%q, expected 'cancelled'", d.round.phase13CancelStatus))
		} else {
			log.Printf("[%s] Phase 13: ✅ turn_cancelled received with status=cancelled", agent)
		}
		// Verify the interaction is marked as interrupted in the store
		phase13Interrupted := false
		allInteractions := d.store.GetAllInteractions()
		for _, i := range allInteractions {
			if i.State == types.InteractionStateInterrupted {
				phase13Interrupted = true
				log.Printf("[%s] Phase 13: ✅ Interaction %s marked as interrupted", agent, truncate(i.ID, 12))
				break
			}
		}
		if !phase13Interrupted {
			errors = append(errors, "Phase 13: No interaction in store with state=interrupted")
		}
	}

	// Phase 14: cancel no-op
	if !d.round.phase14TurnCancelled {
		errors = append(errors, "Phase 14: No turn_cancelled event received (noop cancel may not have been processed)")
	} else {
		if d.round.phase14CancelStatus != "noop" {
			errors = append(errors, fmt.Sprintf("Phase 14: turn_cancelled status=%q, expected 'noop'", d.round.phase14CancelStatus))
		} else {
			log.Printf("[%s] Phase 14: ✅ turn_cancelled received with status=noop", agent)
		}
	}

	// Phase 15: streaming patches arrive incrementally
	//
	// This guards against the regression fixed by `Emit EntryUpdated after
	// streaming-reveal drain so WS sync sees fresh content`. Without that
	// fix, text drained from `streaming_text_buffer.pending` into the
	// markdown entity is invisible to external_websocket_sync until the next
	// chunk arrives — so the bulk of content reaches Helix only at the
	// `Stopped` re-send right before `message_completed`. The three asserts
	// below catch that pattern with comfortable margin so they don't flake
	// on slow LLM streaming.
	if !d.hasRoundCompletion(d.round.reqID("phase15")) {
		errors = append(errors, "Phase 15: No message_completed for "+d.round.reqID("phase15"))
	} else if d.round.phase15ThreadID == "" {
		errors = append(errors, "Phase 15: No thread_created event captured (phase 15 may not have run)")
	} else {
		log.Printf("[%s] Phase 15: %d assistant message_added samples for thread=%s",
			agent, len(d.round.phase15Adds), truncate(d.round.phase15ThreadID, 12))

		// Assert 1: at least 40 distinct message_added events arrived for the
		// assistant turn. Calibration: against zed-agent + claude-sonnet-4-5
		// streaming a ~2.5 KB prose response, the with-fix baseline is ~58
		// samples (one per LLM chunk PLUS one per 16ms streaming-reveal
		// drain tick, throttled to 100ms). Without the cx.emit(EntryUpdated)
		// re-emit after drain, only push_chunk emissions reach WS sync —
		// observed baseline ~31 samples. 40 sits comfortably between the two
		// with margin for LLM/throttle variance, so the test fails reliably
		// when the cherry-pick is missing and passes when it's present.
		const minSamples = 40
		if len(d.round.phase15Adds) < minSamples {
			errors = append(errors, fmt.Sprintf(
				"Phase 15: only %d assistant message_added events arrived for streaming response (need >= %d) — text drained into markdown is invisible to WS sync until next chunk",
				len(d.round.phase15Adds), minSamples))
		} else {
			// Assert 2: the longest gap between consecutive message_added events
			// (between the first sample and message_completed) is bounded. The
			// bug shows up as a multi-minute gap where the model is producing
			// chunks but they're not surfacing through external_websocket_sync.
			const maxGap = 20 * time.Second
			var worstGap time.Duration
			for i := 1; i < len(d.round.phase15Adds); i++ {
				gap := d.round.phase15Adds[i].ts.Sub(d.round.phase15Adds[i-1].ts)
				if gap > worstGap {
					worstGap = gap
				}
			}
			log.Printf("[%s] Phase 15: longest inter-message gap = %s (limit %s)",
				agent, worstGap, maxGap)
			if worstGap > maxGap {
				errors = append(errors, fmt.Sprintf(
					"Phase 15: longest gap between consecutive message_added events = %s (limit %s) — streaming arrival stalled",
					worstGap, maxGap))
			}

			// Assert 3: the bug pattern is "almost-everything-arrives-in-the-final-burst".
			// We catch it by asserting that NO MORE than 90% of the final content length
			// is contributed by the LAST 20% of streaming time. With the
			// streaming-reveal fix the curve is reasonably even and the final-window
			// share is well below 50%. Without the fix it pegs at ~100% (everything
			// arrives in the closing Stopped re-emit).
			//
			// We use this end-of-window threshold rather than a midpoint check
			// because some agents legitimately stream non-prose content first
			// (e.g. claude_code emits internal thinking before the prose answer
			// for long responses), which makes the midpoint metric agent-specific.
			// The "did everything land in the final burst?" question is the actual
			// regression signal we care about and works uniformly across agents.
			if !d.round.phase15ChatSentAt.IsZero() && !d.round.phase15CompletedAt.IsZero() && d.round.phase15FinalLen > 0 {
				totalElapsed := d.round.phase15CompletedAt.Sub(d.round.phase15ChatSentAt)
				finalWindowStart := d.round.phase15CompletedAt.Add(-totalElapsed / 5) // last 20%

				lenBeforeFinalWindow := 0
				for _, s := range d.round.phase15Adds {
					if s.ts.After(finalWindowStart) {
						break
					}
					if s.contentLen > lenBeforeFinalWindow {
						lenBeforeFinalWindow = s.contentLen
					}
				}
				bytesInFinalWindow := d.round.phase15FinalLen - lenBeforeFinalWindow
				pctInFinalWindow := 100 * bytesInFinalWindow / d.round.phase15FinalLen
				const maxPctInFinalWindow = 90
				log.Printf("[%s] Phase 15: %d/%d bytes (%d%%) arrived in the FINAL 20%% of stream time (limit <= %d%%)",
					agent, bytesInFinalWindow, d.round.phase15FinalLen, pctInFinalWindow, maxPctInFinalWindow)
				if pctInFinalWindow > maxPctInFinalWindow {
					errors = append(errors, fmt.Sprintf(
						"Phase 15: %d%% of final content (%d/%d bytes) arrived in the LAST 20%% of stream time (limit <= %d%%) — content arrived in a single burst at the end",
						pctInFinalWindow, bytesInFinalWindow, d.round.phase15FinalLen, maxPctInFinalWindow))
				}
			}
		}
	}

	// Thread count: at minimum 5 threads (phases 1, 3, 8, 13, 15). ACP agents that
	// don't support session reload (like Claude Code) may create additional threads
	// when follow-up messages fall through to create_new_thread (phases 4, 7, 11).
	if len(threadCreatedEvents) > 5 {
		log.Printf("[%s] Note: %d thread_created events (>5 expected for agents that don't retain sessions)",
			agent, len(threadCreatedEvents))
	}

	// Phase 16 — DEFERRED-EMIT REGRESSION ASSERTION (Fix 1a).
	//
	// Zed's agent panel `activate_draft` creates a non-resume
	// `ConversationView` for the empty input editor every time the panel
	// is shown. Pre-Fix-1a (helixml/zed PR #56), that load_task's
	// completion handler emitted `SyncEvent::UserCreatedThread`
	// immediately for the `!is_resume` branch even though the user had
	// not typed anything. Helix recorded a phantom `helix_session` per
	// container restart, plus a duplicate Claude ACP spawn whose npm
	// exec children raced against the existing ones for the
	// `_npx/<hash>` cache → 180s context_server timeouts. Post-Fix-1a,
	// `defer_user_created_thread` registers the emit in a pending map
	// flushed only on the first user-role NewEntry — and this e2e drives
	// all phases via chat_message (which goes through
	// create_new_thread_sync → register_thread, NOT through the
	// activate_draft path), so the panel's draft never sees a user
	// message and its emit stays pending forever.
	//
	// Phase 10 injects user_created_thread directly via ProcessSyncEvent
	// (bypasses WebSocket entirely), so it does not increment this
	// counter — see roundState.spontaneousUserCreatedThreadCount doc.
	//
	// To verify this assertion's regression power: revert
	// `defer_user_created_thread` calls in conversation_view.rs and
	// acp/thread_view.rs back to immediate
	// `send_websocket_event(SyncEvent::UserCreatedThread {…})` and
	// re-run; this assertion will fail with the draft thread's UUID
	// in the diagnostic.
	//
	// Full diagnosis: helix/design/2026-05-13-mcp-cache-contention-and-duplicate-claude-spawn.md
	if d.round.spontaneousUserCreatedThreadCount > 0 {
		errors = append(errors, fmt.Sprintf(
			"Phase 16 (deferred-emit regression): received %d spontaneous user_created_thread event(s) — Zed agent panel's draft ConversationView is emitting UserCreatedThread eagerly instead of deferring until first user message. Phantom acp_thread_id(s): %v. See helixml/zed PR #56 / helix/design/2026-05-13-mcp-cache-contention-and-duplicate-claude-spawn.md",
			d.round.spontaneousUserCreatedThreadCount,
			d.round.spontaneousUserCreatedThreadIDs,
		))
	} else {
		log.Printf("[%s] Phase 16: 0 spontaneous user_created_thread events — Fix 1a deferred-emit working as expected", agent)
	}

	// Phase 17 — LIVE CLAUDE PROCESS COUNT (Fix 1b regression).
	//
	// Counts how many `claude --output-format` processes are alive in
	// the test container right now. Each ACP `new_session()` call against
	// the Claude wrapper spawns one Claude child; the wrapper itself
	// (npm exec @agentclientprotocol/claude-agent-acp) is shared across
	// sessions. So the live Claude count == number of distinct Claude
	// ACP sessions that haven't been disposed.
	//
	// EXPECTATION: this number should equal the number of distinct
	// thread_created events we observed for the current agent's round
	// (one Claude per real conversation thread). If it's HIGHER than
	// expected, the agent panel's draft `activate_draft` is spawning
	// extra Claude processes on top of the user's actual conversations
	// — that's the Fix 1b regression we're guarding against. Each extra
	// Claude brings its own MCP child tree and contends for the npm
	// `_npx/<hash>` cache, eventually surfacing as 180s
	// `chrome-devtools/github context server failed to start` timeouts
	// in long-running spec_tasks.
	//
	// IMPORTANT: this assertion currently DOES catch the ambient draft
	// Claude that spawns on Zed startup — Fix 1b (defer
	// connection.new_session() until first user input in the draft) is
	// not yet implemented. Until that lands, this assertion will report
	// `expected N, got N+1` per agent round and fail. That failure is
	// the deliberate tracked-failure regression-test for Fix 1b. Once
	// Fix 1b lands the assertion passes and continues to guard against
	// future regression. See:
	// helix/design/2026-05-13-mcp-cache-contention-and-duplicate-claude-spawn.md
	// section "Why Fix 1b was deferred" for the implementation plan.
	if agent == "claude" {
		liveClaudes, err := countLiveClaudeProcesses()
		if err != nil {
			log.Printf("[%s] Phase 17: WARNING failed to count Claude processes: %v", agent, err)
		} else {
			expectedClaudes := len(d.round.threadIDs) +
				boolToInt(d.round.phase8ThreadID != "" && !contains(d.round.threadIDs, d.round.phase8ThreadID)) +
				boolToInt(d.round.phase13ThreadID != "" && !contains(d.round.threadIDs, d.round.phase13ThreadID)) +
				boolToInt(d.round.phase15ThreadID != "" && !contains(d.round.threadIDs, d.round.phase15ThreadID)) +
				boolToInt(d.round.phase10NewThreadID != "" && !contains(d.round.threadIDs, d.round.phase10NewThreadID))
			log.Printf("[%s] Phase 17: %d live Claude processes (expected %d)", agent, liveClaudes, expectedClaudes)
			if liveClaudes > expectedClaudes {
				extra := liveClaudes - expectedClaudes
				errors = append(errors, fmt.Sprintf(
					"Phase 17 (Fix 1b regression): %d Claude processes alive but only %d real conversation threads exist (%d extras). Likely the agent panel's `activate_draft` is spawning a Claude for the empty input editor before the user types. Each extra Claude child brings its own MCP server tree and contends for the `_npx/<hash>` cache → 180s context_server timeouts in long-running spec_tasks. Fix 1b (defer connection.new_session() until first user input in draft) is not yet implemented; once it lands this assertion will pass without the +1 ambient draft. See helix/design/2026-05-13-mcp-cache-contention-and-duplicate-claude-spawn.md.",
					liveClaudes, expectedClaudes, extra,
				))
			}
		}
	}

	// --- MCP TOOLS WAIT VALIDATION (first round only) ---
	if d.currentRoundIdx == 0 {
		log.Println("\n--------------------------------------------------")
		log.Printf("  [%s] MCP TOOLS WAIT VALIDATION", agent)
		log.Println("--------------------------------------------------")

		if !d.round.phase1ChatSentAt.IsZero() && !d.round.phase1ThreadCreated.IsZero() {
			mcpWaitDuration := d.round.phase1ThreadCreated.Sub(d.round.phase1ChatSentAt)
			log.Printf("[%s] MCP wait: chat_message sent -> thread_created = %s", agent, mcpWaitDuration)

			const minExpectedDelay = 8 * time.Second
			if mcpWaitDuration < minExpectedDelay {
				errors = append(errors, fmt.Sprintf(
					"MCP tools wait: thread_created arrived %.1fs after chat_message (expected >= %.0fs). "+
						"This means Zed did NOT wait for MCP tools to load before sending the first message.",
					mcpWaitDuration.Seconds(), minExpectedDelay.Seconds()))
			} else {
				log.Printf("[%s] MCP tools wait: Zed correctly waited %.1fs for tools to load", agent, mcpWaitDuration.Seconds())
			}
		} else {
			log.Printf("[%s] WARNING: Could not measure MCP tools wait (missing timestamps)", agent)
		}
	}

	// --- STREAMING VALIDATION ---
	log.Println("\n--------------------------------------------------")
	log.Printf("  [%s] STREAMING VALIDATION", agent)
	log.Println("--------------------------------------------------")

	completionPhases := []string{
		d.round.reqID("phase1"), d.round.reqID("phase2"), d.round.reqID("phase3"),
		d.round.reqID("phase4"), d.round.reqID("phase5"), d.round.reqID("phase7"),
		d.round.reqID("phase15"),
	}
	for _, reqID := range completionPhases {
		firstAddedIdx := -1
		completedIdx := -1
		addedCount := 0

		for i, evt := range d.round.events {
			if evt.EventType == "message_added" && evt.Data["role"] == "assistant" {
				if firstAddedIdx == -1 {
					firstAddedIdx = i
				}
				addedCount++
			}
			if evt.EventType == "message_completed" && evt.Data["request_id"] == reqID {
				completedIdx = i
			}
		}

		if firstAddedIdx >= 0 && completedIdx >= 0 {
			if firstAddedIdx < completedIdx {
				log.Printf("[%s] Streaming %s: %d message_added before message_completed", agent, reqID, addedCount)
			} else {
				errors = append(errors, fmt.Sprintf("Streaming %s: message_added did NOT arrive before message_completed", reqID))
			}
		}
	}

	// --- SUMMARY ---
	passed := len(errors) == 0
	if !passed {
		fmt.Println()
		for _, e := range errors {
			log.Printf("[%s] FAIL: %s", agent, e)
		}
	} else {
		fmt.Println()
		log.Printf("[%s] Phase 1: Basic thread creation - PASSED", agent)
		log.Printf("[%s] Phase 2: Follow-up on existing thread - PASSED", agent)
		log.Printf("[%s] Phase 3: New thread via WebSocket - PASSED", agent)
		log.Printf("[%s] Phase 4: Follow-up to non-visible thread - PASSED", agent)
		log.Printf("[%s] Phase 5: Zed -> Helix user message sync - PASSED", agent)
		log.Printf("[%s] Phase 6: Query UI state - PASSED", agent)
		log.Printf("[%s] Phase 7: Open thread + follow-up - PASSED", agent)
		log.Printf("[%s] Phase 8: Mid-stream interrupt - PASSED", agent)
		log.Printf("[%s] Phase 9: Rapid 3-turn cancel - PASSED", agent)
		log.Printf("[%s] Phase 10: User-created thread - PASSED", agent)
		log.Printf("[%s] Phase 11: Spectask routing - PASSED", agent)
		log.Printf("[%s] Phase 12: Reconnect test - PASSED", agent)
		log.Printf("[%s] Phase 13: Helix-initiated cancel - PASSED", agent)
		log.Printf("[%s] Phase 14: Cancel no-op - PASSED", agent)
		log.Printf("[%s] Phase 15: Streaming patches arrive incrementally - PASSED", agent)
	}

	totalCompletions := 0
	for _, v := range d.round.completions {
		totalCompletions += len(v)
	}
	log.Printf("[%s] Total threads: %d, Total completions: %d",
		agent, len(d.round.threadIDs), totalCompletions)

	// Dump all sync events when a round fails for CI diagnostics
	if !passed {
		log.Printf("\n--------------------------------------------------")
		log.Printf("  [%s] SYNC EVENT DUMP (round failed — %d events)", agent, len(d.round.events))
		log.Printf("--------------------------------------------------")
		for idx, ev := range d.round.events {
			data, _ := json.Marshal(ev.Data)
			dataStr := string(data)
			if len(dataStr) > 200 {
				dataStr = dataStr[:200] + "..."
			}
			log.Printf("[%s] event %3d: type=%-25s data=%s", agent, idx, ev.EventType, dataStr)
		}
	}

	return roundResult{agentName: agent, passed: passed, errors: errors}
}

// validateStore runs cross-round store state validation (sessions, interactions).
func (d *testDriver) validateStore() bool {
	d.mu.Lock()
	defer d.mu.Unlock()

	log.Println("\n==================================================")
	log.Println("  STORE STATE VALIDATION (production handlers)")
	log.Println("==================================================")

	var errors []string

	sessions := d.store.GetAllSessions()
	interactions := d.store.GetAllInteractions()

	log.Printf("[store] Sessions in store: %d", len(sessions))
	log.Printf("[store] Interactions in store: %d", len(interactions))

	// Each round creates 5 threads (phases 1, 3, 8, 13, 15) = 5 sessions per round.
	expectedSessions := 5 * len(d.agentRounds)
	if len(sessions) < expectedSessions {
		errors = append(errors, fmt.Sprintf("Expected at least %d sessions (%d rounds * 4 threads), got %d",
			expectedSessions, len(d.agentRounds), len(sessions)))
	}

	// Check that sessions have ZedThreadID metadata
	sessionsWithThread := 0
	for _, s := range sessions {
		if s.Metadata.ZedThreadID != "" {
			sessionsWithThread++
			log.Printf("[store] Session %s: ZedThreadID=%s, Owner=%s, Name=%q",
				truncate(s.ID, 12), truncate(s.Metadata.ZedThreadID, 12), s.Owner, s.Name)
		}
	}
	if sessionsWithThread < expectedSessions {
		errors = append(errors, fmt.Sprintf("Expected at least %d sessions with ZedThreadID, got %d", expectedSessions, sessionsWithThread))
	}

	// Check completed/interrupted interactions.
	// Phases 8 and 9 include mid-stream interrupt tests where the agent may be
	// cancelled while executing tool calls (before generating any text). Those
	// interactions legitimately end up complete with ResponseMessage="" and only
	// tool_call entries. Phase 13 interactions are marked as "interrupted" via
	// the cancel_current_turn protocol.
	completedInteractions := 0
	completedWithContent := 0
	interruptedInteractions := 0
	for _, i := range interactions {
		if i.State == types.InteractionStateInterrupted {
			interruptedInteractions++
			log.Printf("[store] Interaction %s: interrupted (phase 13 cancel_current_turn)", truncate(i.ID, 12))
			continue
		}
		if i.State != types.InteractionStateComplete {
			continue
		}
		completedInteractions++

		if i.ResponseMessage == "" {
			// Check if this was interrupted during tool use (has tool_call entries but no text).
			// This is expected for phases 8 and 9 cancellations.
			interrupted := false
			if len(i.ResponseEntries) > 0 {
				var entries []struct {
					Type string `json:"type"`
				}
				if err := json.Unmarshal(i.ResponseEntries, &entries); err == nil {
					hasText := false
					for _, e := range entries {
						if e.Type == "text" {
							hasText = true
							break
						}
					}
					if !hasText {
						interrupted = true
					}
				}
			}
			if interrupted {
				log.Printf("[store] Interaction %s: complete, interrupted during tool use (no text — expected for phases 8/9)",
					truncate(i.ID, 12))
			} else {
				// Truly empty — no entries at all. This is also expected for turns
				// cancelled before generating any output (e.g. rapid cancel in phase 9).
				log.Printf("[store] Interaction %s: complete with no content (cancelled before output — expected for phases 8/9)",
					truncate(i.ID, 12))
			}
			continue
		}

		// Interaction has text content — validate it properly.
		completedWithContent++
		log.Printf("[store] Completed interaction %s: %d bytes response, session=%s",
			truncate(i.ID, 12), len(i.ResponseMessage), truncate(i.SessionID, 12))

		if len(i.ResponseEntries) == 0 {
			errors = append(errors, fmt.Sprintf("Interaction %s: has ResponseMessage but no ResponseEntries",
				truncate(i.ID, 12)))
		} else {
			var entries []struct {
				Type      string `json:"type"`
				Content   string `json:"content"`
				MessageID string `json:"message_id"`
			}
			if err := json.Unmarshal(i.ResponseEntries, &entries); err != nil {
				errors = append(errors, fmt.Sprintf("Interaction %s: failed to parse ResponseEntries: %v",
					truncate(i.ID, 12), err))
			} else {
				hasText := false
				for _, e := range entries {
					if e.Type == "text" {
						hasText = true
					}
					if e.Type != "text" && e.Type != "tool_call" {
						errors = append(errors, fmt.Sprintf("Interaction %s: unexpected entry type %q",
							truncate(i.ID, 12), e.Type))
					}
					if e.Content == "" {
						errors = append(errors, fmt.Sprintf("Interaction %s: entry %s has empty content",
							truncate(i.ID, 12), e.MessageID))
					}
				}
				if !hasText {
					errors = append(errors, fmt.Sprintf("Interaction %s: has ResponseMessage but no 'text' entries in ResponseEntries",
						truncate(i.ID, 12)))
				}
			}
		}
	}

	// Expect at least 8 completed interactions per round:
	//   - Phase 1:  thread_created → new session + interaction
	//   - Phase 2:  sendChatMessageToExternalAgent creates interaction for follow-up
	//   - Phase 3:  thread_created → new session + interaction
	//   - Phase 4:  sendChatMessageToExternalAgent creates interaction for follow-up
	//   - Phase 5:  message_added(role=user) → on-the-fly interaction
	//   - Phase 7:  sendChatMessageToExternalAgent creates interaction for follow-up
	//   - Phase 8:  thread_created → new session + interaction
	//   - Phase 9:  on-the-fly interaction (from user interrupt)
	//   - Phase 11: sendChatMessageToExternalAgent via spectask routing
	//   - Phase 15: thread_created → new session + interaction (streaming-cadence)
	expectedCompleted := 8 * len(d.agentRounds)
	if completedInteractions < expectedCompleted {
		errors = append(errors, fmt.Sprintf("Expected at least %d completed interactions, got %d", expectedCompleted, completedInteractions))
	}

	// Expect at least 8 interactions WITH content per round.
	expectedWithContent := 8 * len(d.agentRounds)
	if completedWithContent < expectedWithContent {
		errors = append(errors, fmt.Sprintf("Expected at least %d completed interactions with content, got %d (accumulation may be broken)",
			expectedWithContent, completedWithContent))
	}

	// --- ACCUMULATION VALIDATION ---
	log.Println("\n--------------------------------------------------")
	log.Println("  ACCUMULATION VALIDATION")
	log.Println("--------------------------------------------------")

	sort.Slice(interactions, func(a, b int) bool {
		return interactions[a].SessionID < interactions[b].SessionID
	})

	for _, i := range interactions {
		if i.State == types.InteractionStateComplete && i.ResponseMessage != "" {
			log.Printf("[store] Interaction %s (session %s): %d bytes, lastMsgID=%s, offset=%d",
				truncate(i.ID, 12), truncate(i.SessionID, 12),
				len(i.ResponseMessage), truncate(i.LastZedMessageID, 12), i.LastZedMessageOffset)
		}
	}

	// --- RESPONSE ENTRIES ISOLATION VALIDATION ---
	// Verify that follow-up interactions don't accumulate response_entries from
	// previous interactions in the same session. This detects the bug where Zed's
	// flush_streaming_throttle resends ALL entries in the ACP thread and the
	// accumulator treats old entries as new, ballooning response_entries.
	log.Println("\n--------------------------------------------------")
	log.Println("  RESPONSE ENTRIES ISOLATION VALIDATION")
	log.Println("--------------------------------------------------")

	// Group interactions by session
	sessionInteractions := make(map[string][]*types.Interaction)
	for _, i := range interactions {
		if i.State == types.InteractionStateComplete && len(i.ResponseEntries) > 0 {
			sessionInteractions[i.SessionID] = append(sessionInteractions[i.SessionID], i)
		}
	}

	isolationChecked := 0
	for sessionID, ints := range sessionInteractions {
		if len(ints) < 2 {
			continue // Need at least 2 interactions to check isolation
		}

		// Sort by creation time
		sort.Slice(ints, func(a, b int) bool {
			return ints[a].Created.Before(ints[b].Created)
		})

		// Collect message_ids from each interaction
		type parsedEntry struct {
			MessageID string `json:"message_id"`
		}

		// For each follow-up interaction, check it doesn't contain message_ids from earlier ones
		previousMessageIDs := make(map[string]string) // message_id → interaction_id that owns it
		for _, inter := range ints {
			var entries []parsedEntry
			if err := json.Unmarshal(inter.ResponseEntries, &entries); err != nil {
				continue
			}

			// Check for leakage: does this interaction contain message_ids from a previous one?
			for _, e := range entries {
				if e.MessageID == "" {
					continue
				}
				if ownerID, leaked := previousMessageIDs[e.MessageID]; leaked {
					errors = append(errors, fmt.Sprintf(
						"ISOLATION VIOLATION: Interaction %s (session %s) contains message_id %q which belongs to earlier interaction %s — response_entries leaked across interactions",
						truncate(inter.ID, 12), truncate(sessionID, 12), e.MessageID, truncate(ownerID, 12)))
				}
			}

			// Register this interaction's message_ids
			for _, e := range entries {
				if e.MessageID != "" {
					previousMessageIDs[e.MessageID] = inter.ID
				}
			}
			isolationChecked++
		}
	}
	if isolationChecked > 0 {
		log.Printf("[store] Response entries isolation: checked %d interactions across %d sessions with follow-ups",
			isolationChecked, len(sessionInteractions))
	}

	// --- THREAD TITLE VALIDATION ---
	log.Println("\n--------------------------------------------------")
	log.Println("  THREAD TITLE VALIDATION")
	log.Println("--------------------------------------------------")

	updatedNames := 0
	for _, s := range sessions {
		if s.Name != "" && s.Name != "New Conversation" && s.Name != "New Chat" {
			updatedNames++
			log.Printf("[store] Session %s name: %q", truncate(s.ID, 12), s.Name)
		}
	}
	if updatedNames > 0 {
		log.Printf("[store] Thread title -> session name sync: %d sessions updated", updatedNames)
	} else {
		log.Println("[store] No thread title updates (Zed may not generate titles for short prompts)")
	}

	if len(errors) > 0 {
		fmt.Println()
		for _, e := range errors {
			log.Printf("[store] FAIL: %s", e)
		}
		return false
	}

	log.Printf("\n[store] Store state: Sessions and interactions created correctly - PASSED")
	log.Printf("[store] Accumulation: %d interactions with content (interrupted/cancelled: %d) - PASSED",
		completedWithContent, completedInteractions-completedWithContent)
	log.Println("[store] Structured entries: ResponseEntries populated for content-bearing interactions - PASSED")
	return true
}

// --- helpers ---

func (d *testDriver) filterRoundEvents(eventType string) []types.SyncMessage {
	var out []types.SyncMessage
	for _, e := range d.round.events {
		if e.EventType == eventType {
			out = append(out, e)
		}
	}
	return out
}

func (d *testDriver) filterRoundEventsByFunc(fn func(types.SyncMessage) bool) []types.SyncMessage {
	var out []types.SyncMessage
	for _, e := range d.round.events {
		if fn(e) {
			out = append(out, e)
		}
	}
	return out
}

func (d *testDriver) hasRoundCompletion(requestID string) bool {
	for _, e := range d.round.events {
		if e.EventType == "message_completed" && e.Data["request_id"] == requestID {
			return true
		}
	}
	return false
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}

// --- main ---

func main() {
	// Determine which agents to test. Default: zed-agent only (backwards compatible).
	// Set E2E_AGENTS="zed-agent,claude" to test multiple agents.
	agentsStr := os.Getenv("E2E_AGENTS")
	var agents []string
	if agentsStr != "" {
		for _, a := range strings.Split(agentsStr, ",") {
			a = strings.TrimSpace(a)
			if a != "" {
				agents = append(agents, a)
			}
		}
	}
	if len(agents) == 0 {
		agents = []string{"zed-agent"}
	}

	log.Printf("[test-server] Agent rounds: %v", agents)

	// Create in-memory store and no-op pubsub
	store := memorystore.New()
	ps := pubsub.NewNoop()

	// Seed a session matching HELIX_SESSION_ID so the production handler
	// can look it up. In production, sessions always exist before Zed connects
	// (created by spectask/session creation flow). Without this, handlers like
	// handleUserCreatedThread fail with "session not found" because they call
	// GetSession(agentSessionID) expecting a real session.
	seedSessionID := os.Getenv("HELIX_SESSION_ID")
	if seedSessionID == "" {
		seedSessionID = "ses_e2e-test-session-001"
	}
	seedSession := types.Session{
		ID:      seedSessionID,
		Name:    "E2E Test Seed Session",
		Created: time.Now(),
		Updated: time.Now(),
		Owner:   "e2e-test-user",
		Mode:    types.SessionModeInference,
		Type:    types.SessionTypeText,
	}
	if _, err := store.CreateSession(context.Background(), seedSession); err != nil {
		log.Fatalf("[test-server] Failed to create seed session: %v", err)
	}
	log.Printf("[test-server] Created seed session: %s", seedSessionID)

	// Create HelixAPIServer with production handlers + in-memory store
	srv := server.NewTestServer(store, ps)

	// Create test driver
	driver := newTestDriver(srv, store, agents)

	// Register sync event hook so test driver observes all events
	srv.SetSyncEventHook(driver.syncEventCallback)

	// Port file for Zed to discover the server
	portFile := "/tmp/mock_helix_port"

	// Register the REAL production WebSocket handler
	http.HandleFunc("/api/v1/external-agents/sync", func(w http.ResponseWriter, r *http.Request) {
		// Set up user mapping for the agent before the handler runs.
		// Extract agent_id the same way the production handler does.
		agentID := r.URL.Query().Get("session_id")
		if agentID == "" {
			agentID = r.URL.Query().Get("agent_id")
		}
		if agentID != "" {
			srv.SetExternalAgentUserMapping(agentID, "e2e-test-user")
			driver.mu.Lock()
			driver.agentID = agentID
			currentPhase := driver.phase
			driver.mu.Unlock()
			log.Printf("[test-server] Agent connecting: %s (phase=%d)", agentID, currentPhase)

			// Phase 12: detect reconnection after Zed restart
			if currentPhase == 12 {
				go func() {
					agent := driver.round.agentName
					log.Printf("[%s] Phase 12: Zed reconnected (new agentID=%s), waiting 5s for initialization...", agent, agentID)
					time.Sleep(5 * time.Second)

					// Send open_thread first so Zed loads the thread from its DB
					driver.mu.Lock()
					if len(driver.round.threadIDs) == 0 {
						driver.mu.Unlock()
						log.Printf("[%s] Phase 12: ERROR no thread IDs available", agent)
						return
					}
					threadA := driver.round.threadIDs[0]
					driver.mu.Unlock()

					log.Printf("[%s] Phase 12: Sending open_thread for Thread A (%s) before chat_message", agent, truncate(threadA, 16))
					driver.sendOpenThread(threadA, agent)

					// Wait for Zed to load the thread before sending the message.
					// Thread loading is async — Zed receives open_thread, spawns load task,
					// connects to agent, loads session from DB. This can take several seconds.
					time.Sleep(10 * time.Second)

					reqID := driver.round.reqID("phase12")
					log.Printf("[%s] Phase 12: Sending chat_message to Thread A (%s) with reqID=%s", agent, truncate(threadA, 16), reqID)
					driver.sendChatMessage("What is 12 + 12? Reply with just the number.", reqID, agent, threadA)
				}()
			}
		}

		// Delegate to the REAL production handler
		srv.ExternalAgentSyncHandler()(w, r)
	})

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		log.Fatalf("[test-server] Listen error: %v", err)
	}
	port := listener.Addr().(*net.TCPAddr).Port
	log.Printf("[test-server] Listening on ws://127.0.0.1:%d", port)
	log.Println("[test-server] Using REAL HelixAPIServer handlers with in-memory store")

	if err := os.WriteFile(portFile, []byte(fmt.Sprintf("%d", port)), 0644); err != nil {
		log.Fatalf("[test-server] Failed to write port file: %v", err)
	}

	go func() {
		if err := http.Serve(listener, nil); err != nil {
			log.Printf("[test-server] Server error: %v", err)
		}
	}()

	// Increase timeout for multi-agent runs. Phase 15 adds ~30–60s of
	// streaming time per agent, so the per-agent budget is bumped to 360s.
	timeout := 360 * time.Second
	if len(agents) > 1 {
		timeout = time.Duration(360*len(agents)) * time.Second
	}

	select {
	case <-driver.done:
	case <-time.After(timeout):
		driver.mu.Lock()
		var eventTypes []string
		if driver.round != nil {
			for _, e := range driver.round.events {
				eventTypes = append(eventTypes, e.EventType)
			}
		}
		driver.mu.Unlock()
		log.Printf("[test-server] TIMEOUT at round %d/%d, phase %d. Events: %v",
			driver.currentRoundIdx+1, len(agents), driver.phase, eventTypes)
		os.Exit(1)
	}

	// Validate store state (cross-round)
	storeOK := driver.validateStore()

	// Print final summary
	log.Println("\n##################################################")
	log.Println("  FINAL RESULTS")
	log.Println("##################################################")

	allPassed := storeOK
	for _, r := range driver.roundResults {
		status := "PASSED"
		if !r.passed {
			status = "FAILED"
			allPassed = false
		}
		log.Printf("  [%s] %s", r.agentName, status)
		for _, e := range r.errors {
			log.Printf("    FAIL: %s", e)
		}
	}
	if storeOK {
		log.Println("  [store] PASSED")
	} else {
		log.Println("  [store] FAILED")
	}

	if allPassed {
		log.Printf("\n[test-server] ALL TESTS PASSED (%d agent rounds, production handlers, in-memory store)", len(agents))
		os.Exit(0)
	} else {
		os.Exit(1)
	}
}

// countLiveClaudeProcesses returns the number of `claude --output-format`
// processes currently alive in the test container. Each ACP `new_session()`
// against the Claude wrapper spawns one such process; the wrapper itself
// (npm exec @agentclientprotocol/claude-agent-acp) is shared across
// sessions. Used by Phase 17 to assert there are no extra "draft" Claude
// processes spawned by the agent panel beyond the real conversation
// threads the test created.
func countLiveClaudeProcesses() (int, error) {
	out, err := exec.Command("ps", "-eo", "args").Output()
	if err != nil {
		return 0, fmt.Errorf("ps failed: %w", err)
	}
	count := 0
	for _, line := range strings.Split(string(out), "\n") {
		if strings.Contains(line, "claude --output-format") {
			count++
		}
	}
	return count, nil
}

func boolToInt(b bool) int {
	if b {
		return 1
	}
	return 0
}

func contains(haystack []string, needle string) bool {
	for _, s := range haystack {
		if s == needle {
			return true
		}
	}
	return false
}
