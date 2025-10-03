#!/usr/bin/env node

/**
 * Mock External System Server for Testing Zed WebSocket Protocol
 *
 * This server simulates an external system (like Helix) for testing
 * Zed's WebSocket protocol implementation.
 *
 * Protocol (per WEBSOCKET_PROTOCOL_SPEC.md):
 * - Receives: thread_created, message_added, message_completed from Zed
 * - Sends: chat_message to Zed
 * - Zed is stateless - only knows acp_thread_id
 * - External system maintains session → acp_thread_id mapping
 *
 * Features:
 * - HTTP API endpoints
 * - WebSocket real-time sync
 * - Mock conversation contexts
 * - Simulated AI responses
 */

const express = require('express');
const WebSocket = require('ws');
const cors = require('cors');
const { v4: uuidv4 } = require('uuid');

// Configuration
const HTTP_PORT = 8081;
const WS_PORT = 8080;
const MOCK_DATA = {
    sessions: new Map(),
    contexts: new Map(),
    messages: new Map(),
};

// Mock data helpers
function createMockSession(sessionId) {
    return {
        id: sessionId,
        createdAt: new Date().toISOString(),
        lastActive: new Date().toISOString(),
        contexts: [],
        metadata: {
            clientType: 'zed-integration',
            version: '0.1.0'
        }
    };
}

function createMockContext(title = 'Mock Conversation') {
    const contextId = uuidv4();
    const context = {
        id: contextId,
        title,
        messages: [],
        createdAt: new Date().toISOString(),
        updatedAt: new Date().toISOString(),
        status: 'active',
        metadata: {}
    };
    MOCK_DATA.contexts.set(contextId, context);
    return context;
}

function createMockMessage(contextId, content, role = 'assistant') {
    const messageId = Date.now();
    const message = {
        id: messageId,
        contextId,
        content,
        role,
        createdAt: new Date().toISOString(),
        status: 'completed',
        metadata: {}
    };
    
    if (!MOCK_DATA.messages.has(contextId)) {
        MOCK_DATA.messages.set(contextId, []);
    }
    MOCK_DATA.messages.get(contextId).push(message);
    
    // Update context
    const context = MOCK_DATA.contexts.get(contextId);
    if (context) {
        context.messages.push(messageId);
        context.updatedAt = new Date().toISOString();
    }
    
    return message;
}

// Express HTTP server
const app = express();

app.use(cors());
app.use(express.json());

// Middleware for logging
app.use((req, res, next) => {
    console.log(`[HTTP] ${req.method} ${req.path}`, req.body ? JSON.stringify(req.body).substring(0, 100) + '...' : '');
    next();
});

// Health check
app.get('/health', (req, res) => {
    res.json({
        status: 'ok',
        service: 'mock-helix-server',
        version: '1.0.0',
        timestamp: new Date().toISOString(),
        stats: {
            sessions: MOCK_DATA.sessions.size,
            contexts: MOCK_DATA.contexts.size,
            totalMessages: Array.from(MOCK_DATA.messages.values()).reduce((sum, msgs) => sum + msgs.length, 0)
        }
    });
});

// Session management
app.get('/api/v1/sessions/:sessionId', (req, res) => {
    const { sessionId } = req.params;
    
    if (!MOCK_DATA.sessions.has(sessionId)) {
        MOCK_DATA.sessions.set(sessionId, createMockSession(sessionId));
    }
    
    const session = MOCK_DATA.sessions.get(sessionId);
    res.json(session);
});

// Thread/Context management
app.get('/api/v1/threads', (req, res) => {
    const threads = Array.from(MOCK_DATA.contexts.values()).map(context => ({
        id: context.id,
        title: context.title,
        message_count: context.messages.length,
        created_at: context.createdAt,
        updated_at: context.updatedAt,
        status: context.status
    }));
    
    res.json({ threads });
});

app.post('/api/v1/threads', (req, res) => {
    const { title = 'New Mock Thread', metadata = {} } = req.body;
    const context = createMockContext(title);
    context.metadata = { ...context.metadata, ...metadata };
    
    console.log(`[MOCK] Created new thread: ${context.id} (${title})`);
    
    res.status(201).json({
        id: context.id,
        title: context.title,
        created_at: context.createdAt,
        status: context.status
    });
});

app.get('/api/v1/threads/:threadId', (req, res) => {
    const { threadId } = req.params;
    const context = MOCK_DATA.contexts.get(threadId);
    
    if (!context) {
        return res.status(404).json({ error: 'Thread not found' });
    }
    
    const messages = MOCK_DATA.messages.get(threadId) || [];
    
    res.json({
        id: context.id,
        title: context.title,
        created_at: context.createdAt,
        updated_at: context.updatedAt,
        status: context.status,
        messages: messages.map(msg => ({
            id: msg.id,
            content: msg.content,
            role: msg.role,
            created_at: msg.createdAt,
            status: msg.status
        }))
    });
});

app.post('/api/v1/threads/:threadId/messages', (req, res) => {
    const { threadId } = req.params;
    const { content, role = 'user', metadata = {} } = req.body;
    
    const context = MOCK_DATA.contexts.get(threadId);
    if (!context) {
        return res.status(404).json({ error: 'Thread not found' });
    }
    
    // Add user message
    const userMessage = createMockMessage(threadId, content, role);
    console.log(`[MOCK] Added message to ${threadId}: ${content.substring(0, 50)}...`);
    
    // Simulate AI response after a short delay
    setTimeout(() => {
        const aiResponses = [
            "I understand your question. Let me help you with that.",
            "That's an interesting point. Here's what I think...",
            "Based on the context, I would suggest the following approach:",
            "Let me analyze that for you and provide some insights.",
            "I can help you with that task. Here's my recommendation:",
            "Good question! Here's how I would approach this problem:"
        ];
        
        const randomResponse = aiResponses[Math.floor(Math.random() * aiResponses.length)];
        const aiMessage = createMockMessage(threadId, randomResponse, 'assistant');
        
        console.log(`[MOCK] AI responded to ${threadId}: ${randomResponse}`);
        
        // Broadcast to WebSocket clients
        broadcastToWebSocketClients({
            type: 'message_added',
            data: {
                thread_id: threadId,
                message: {
                    id: aiMessage.id,
                    content: aiMessage.content,
                    role: aiMessage.role,
                    created_at: aiMessage.createdAt
                }
            }
        });
    }, 1000 + Math.random() * 2000); // 1-3 second delay
    
    res.status(201).json({
        id: userMessage.id,
        thread_id: threadId,
        content: userMessage.content,
        role: userMessage.role,
        created_at: userMessage.createdAt
    });
});

// External agent sync endpoints
app.get('/api/v1/external-agents/sync', (req, res) => {
    res.json({
        status: 'ready',
        websocket_endpoint: `ws://localhost:${WS_PORT}/sync`,
        supported_features: ['context_sync', 'message_sync', 'real_time_updates']
    });
});

// WebSocket server for real-time sync
const wss = new WebSocket.Server({ 
    port: WS_PORT,
    path: '/sync'
});

const wsClients = new Map();

function broadcastToWebSocketClients(message, excludeClient = null) {
    const messageStr = JSON.stringify({
        ...message,
        timestamp: new Date().toISOString()
    });
    
    wsClients.forEach((client, id) => {
        if (client !== excludeClient && client.readyState === WebSocket.OPEN) {
            client.send(messageStr);
        }
    });
}

wss.on('connection', (ws, req) => {
    const clientId = uuidv4();
    const sessionId = new URL(req.url, `http://${req.headers.host}`).searchParams.get('session_id') || 'unknown';
    
    wsClients.set(clientId, ws);
    
    console.log(`[WS] Client connected: ${clientId} (session: ${sessionId})`);
    
    // Send welcome message
    ws.send(JSON.stringify({
        type: 'welcome',
        data: {
            client_id: clientId,
            session_id: sessionId,
            server_info: {
                name: 'mock-helix-server',
                version: '1.0.0',
                features: ['context_sync', 'message_sync']
            }
        },
        timestamp: new Date().toISOString()
    }));
    
    ws.on('message', (data) => {
        try {
            const message = JSON.parse(data.toString());
            console.log(`[WS] Received from ${clientId}:`, message.event_type || message.type, JSON.stringify(message.data || {}).substring(0, 100));
            
            handleWebSocketMessage(ws, clientId, message);
        } catch (error) {
            console.error(`[WS] Error parsing message from ${clientId}:`, error.message);
            ws.send(JSON.stringify({
                type: 'error',
                data: { error: 'Invalid JSON message' },
                timestamp: new Date().toISOString()
            }));
        }
    });
    
    ws.on('close', () => {
        console.log(`[WS] Client disconnected: ${clientId}`);
        wsClients.delete(clientId);
    });
    
    ws.on('error', (error) => {
        console.error(`[WS] Client ${clientId} error:`, error.message);
    });
    
    // Send periodic ping
    const pingInterval = setInterval(() => {
        if (ws.readyState === WebSocket.OPEN) {
            ws.ping();
        } else {
            clearInterval(pingInterval);
        }
    }, 30000);
});

function handleWebSocketMessage(ws, clientId, message) {
    const messageType = message.type;
    const data = message.data || message;

    switch (messageType) {
        case 'thread_created':
            console.log(`[WS] ✅ Thread created in Zed:`);
            console.log(`     acp_thread_id: ${data.acp_thread_id}`);
            console.log(`     request_id: ${data.request_id}`);

            // External system would store this mapping:
            // sessions[request_id].acp_thread_id = data.acp_thread_id

            // Could send a follow-up message to the thread
            // (not done here to keep test simple)
            break;

        case 'message_added':
            console.log(`[WS] 📝 Message added (streaming):`);
            console.log(`     acp_thread_id: ${data.acp_thread_id}`);
            console.log(`     message_id: ${data.message_id}`);
            console.log(`     content: ${data.content.substring(0, 50)}...`);

            // External system updates its UI with streaming content
            break;

        case 'message_completed':
            console.log(`[WS] ✅ Message completed:`);
            console.log(`     acp_thread_id: ${data.acp_thread_id}`);
            console.log(`     message_id: ${data.message_id}`);
            console.log(`     request_id: ${data.request_id}`);

            // External system marks interaction as complete
            break;

        case 'context_created':
            // Legacy event - deprecated
            console.log(`[WS] ⚠️ Received legacy context_created event (deprecated)`);
            console.log(`[WS] Context created in Zed: ${data.context_id}`);

            // Simulate adding a welcome message to the new context
            setTimeout(() => {
                ws.send(JSON.stringify({
                    type: 'add_message',
                    data: {
                        context_id: data.context_id,
                        content: `Welcome to your new conversation! I'm a mock Helix assistant ready to help.`,
                        role: 'assistant'
                    }
                }));
            }, 1500);
            break;
            
        case 'context_deleted':
            console.log(`[WS] Context deleted in Zed: ${data.context_id}`);
            break;
            
        case 'message_added':
            console.log(`[WS] Message added in Zed: ${data.context_id} - ${data.message_id}`);
            
            // Simulate processing the message and responding
            setTimeout(() => {
                const responses = [
                    "Thanks for your message! I've processed it and here's my response.",
                    "Interesting input! Let me elaborate on that topic.",
                    "I see what you're getting at. Here's my perspective on this.",
                    "Great question! Allow me to provide some additional context.",
                    "I understand. Let me help you explore this further."
                ];
                
                const response = responses[Math.floor(Math.random() * responses.length)];
                
                ws.send(JSON.stringify({
                    type: 'add_message',
                    data: {
                        context_id: data.context_id,
                        content: response,
                        role: 'assistant'
                    }
                }));
            }, 2000);
            break;
            
        case 'ping':
            ws.send(JSON.stringify({
                type: 'pong',
                data: { timestamp: new Date().toISOString() }
            }));
            break;
            
        case 'subscribe':
            console.log(`[WS] Client ${clientId} subscribed to events:`, data.events);
            break;
            
        default:
            console.log(`[WS] Unknown message type from ${clientId}: ${messageType}`);
    }
}

// Start servers
app.listen(HTTP_PORT, () => {
    console.log(`🚀 Mock Helix HTTP Server running on port ${HTTP_PORT}`);
    console.log(`   Health check: http://localhost:${HTTP_PORT}/health`);
    console.log(`   Threads API: http://localhost:${HTTP_PORT}/api/v1/threads`);
});

console.log(`🔌 Mock Helix WebSocket Server running on port ${WS_PORT}`);
console.log(`   WebSocket endpoint: ws://localhost:${WS_PORT}/sync`);

// Graceful shutdown
process.on('SIGINT', () => {
    console.log('\n🛑 Shutting down mock Helix server...');
    
    // Close WebSocket connections
    wsClients.forEach(client => {
        if (client.readyState === WebSocket.OPEN) {
            client.close(1000, 'Server shutting down');
        }
    });
    
    wss.close(() => {
        console.log('✅ WebSocket server closed');
        process.exit(0);
    });
});

// Create some initial mock data
setTimeout(() => {
    const welcomeContext = createMockContext('Welcome to Mock Helix');
    createMockMessage(welcomeContext.id, 'Hello! I\'m your mock Helix assistant. This is a test conversation to verify the integration is working.', 'assistant');
    createMockMessage(welcomeContext.id, 'You can create new conversations, send messages, and I\'ll respond automatically.', 'assistant');
    
    const techContext = createMockContext('Technical Discussion');
    createMockMessage(techContext.id, 'Let\'s discuss some technical topics. What would you like to explore?', 'assistant');
    
    console.log('✅ Created initial mock data');
    console.log(`   - ${MOCK_DATA.contexts.size} contexts created`);
    console.log(`   - ${Array.from(MOCK_DATA.messages.values()).reduce((sum, msgs) => sum + msgs.length, 0)} messages created`);
}, 1000);

console.log('\n📋 To test the integration:');
console.log('1. Configure Zed with helix_integration enabled');
console.log('2. Set helix_url to "localhost:8080" in settings');
console.log('3. Open a project in Zed to trigger integration');
console.log('4. Watch the logs here for real-time communication');
console.log('\n🧪 Test commands:');
console.log(`curl http://localhost:${HTTP_PORT}/health`);
console.log(`curl http://localhost:${HTTP_PORT}/api/v1/threads`);