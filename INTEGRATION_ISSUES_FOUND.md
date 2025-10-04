# Integration Testing Issues - Root Cause Analysis

## Issues Reported:
1. ❌ Agent panel didn't auto-pop up
2. ❌ Claude Pro ad still showing (thought we added disable options)
3. ❌ Message from Helix didn't appear in Zed
4. ❌ No response back to Helix

## Root Causes:

### Issue #1: WebSocket Service Never Connects

**Problem**: The WebSocket client never connects to the external server (Helix).

**Why**: 
1. `external_websocket_sync::init()` is called (line 603 in main.rs) ✅
2. BUT it only sets up globals - doesn't actually connect
3. Connection requires settings to be enabled
4. **Settings default to disabled!**

**Settings Structure**:
```json
{
  "external_sync": {
    "enabled": false,  // ← Defaults to FALSE
    "websocket_sync": {
      "enabled": false,  // ← Defaults to FALSE
      "external_url": "localhost:8080",
      "auth_token": null,
      "use_tls": false
    }
  }
}
```

**Fix Required**:
User must add to their Zed settings.json:
```json
{
  "external_sync": {
    "enabled": true,
    "websocket_sync": {
      "enabled": true,
      "external_url": "localhost:8080"
    }
  }
}
```

**Code Fix Added**: 
- Updated `external_websocket_sync::init()` to check settings
- If enabled, calls `init_websocket_service()` automatically
- Location: external_websocket_sync.rs:490-506

### Issue #2: Thread Handler Never Set Up

**Problem**: Even if WebSocket connects, thread handler isn't initialized.

**Why**:
- `setup_thread_handler()` is called in zed.rs:627-637
- BUT only when workspace is created
- If Zed was already running, workspace already exists
- Handler never gets set up!

**When It Works**:
- Fresh Zed launch → creates workspace → calls setup ✅
- Already running Zed → no new workspace → no setup ❌

**Fix Required**:
Restart Zed after updating code, OR move setup to happen on first workspace creation globally.

### Issue #3: Agent Panel Auto-Open

**Problem**: Panel doesn't auto-open when external agent starts.

**Current Behavior**:
- Zed creates thread in background (headless)
- No UI interaction
- Panel stays closed

**Settings Available**:
```json
{
  "agent": {
    "auto_open_panel": true  // This exists but may not work for external threads
  }
}
```

**Possible Issues**:
1. `auto_open_panel` might only work for UI-initiated threads
2. External thread creation doesn't trigger panel open
3. Need to add explicit panel focus when thread created via WebSocket

**Fix Needed**:
Add code to open/focus agent panel when external thread is created.

### Issue #4: Claude Pro Ad (Onboarding)

**Problem**: Onboarding banner still shows despite options to disable.

**Settings That Exist**:
```json
{
  "agent": {
    "show_onboarding": false  // Disable onboarding banner
  }
}
```

**Also Check**:
```bash
export ZED_SHOW_ONBOARDING=0  // Environment variable
```

**Code Location**: agent_panel.rs:2616-2628 checks these

## Summary of Required Actions:

### To Test WebSocket Integration:

1. **Add to Zed settings.json**:
```json
{
  "external_sync": {
    "enabled": true,
    "websocket_sync": {
      "enabled": true,
      "external_url": "localhost:8080"
    }
  },
  "agent": {
    "auto_open_panel": true,
    "show_onboarding": false
  }
}
```

2. **Restart Zed completely**:
   - Kill all zed processes
   - Start fresh
   - This ensures workspace creation triggers setup_thread_handler()

3. **Verify in logs**:
   - Should see: `🔌 [WEBSOCKET] WebSocket sync is enabled in settings`
   - Should see: `✅ [WEBSOCKET] WebSocket service started`
   - Should see: `🔧 [THREAD_SERVICE] Setting up WebSocket thread handler`
   - Should see: `✅ [ZED] WebSocket thread handler initialized`

4. **Test from Helix**:
   - Send message from Helix
   - Check for: `📨 [THREAD_SERVICE] Received thread creation request`
   - Check for: `🆕 [THREAD_SERVICE] Creating new ACP thread`
   - Check for: `📤 [THREAD_SERVICE] Sent thread_created`

## Quick Debug Commands:

```bash
# Check if WebSocket is connecting
tail -f /home/retro/.local/share/zed/logs/Zed.log | grep WEBSOCKET

# Check if thread handler is set up  
tail -f /home/retro/.local/share/zed/logs/Zed.log | grep THREAD_SERVICE

# Check Helix side
docker logs helix-api 2>&1 | grep -i "zed\|websocket\|external.*agent"
```

## Remaining Code Issues to Fix:

1. **init() should auto-start if enabled** - ✅ FIXED (just committed)
2. **setup_thread_handler timing** - Needs restart or move to global init
3. **Auto-open panel** - Needs implementation
4. **Disable onboarding** - User settings needed

