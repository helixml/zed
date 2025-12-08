//! Settings for External WebSocket Thread Sync

use gpui::App;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExternalSyncSettings {
    /// Whether external WebSocket thread sync is enabled
    #[serde(default)]
    pub enabled: bool,
    
    /// WebSocket sync configuration
    #[serde(default)]
    pub websocket_sync: WebSocketSyncSettings,
    
    /// MCP (Model Context Protocol) configuration  
    #[serde(default)]
    pub mcp: McpSettings,
    
    /// HTTP server configuration for external agents
    #[serde(default)]
    pub server: ServerSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WebSocketSyncSettings {
    /// Enable WebSocket sync with external service
    #[serde(default)]
    pub enabled: bool,

    /// External server URL (without protocol)
    #[serde(default = "default_external_url")]
    pub external_url: String,

    /// Authentication token for external service API
    #[serde(default)]
    pub auth_token: Option<String>,

    /// Use TLS for WebSocket connection
    #[serde(default)]
    pub use_tls: bool,

    /// Skip TLS certificate verification (DANGEROUS - for enterprise internal CAs only)
    /// Set ZED_HELIX_SKIP_TLS_VERIFY=true to enable
    #[serde(default)]
    pub skip_tls_verify: bool,

    /// Auto-reconnect on connection failure
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,

    /// Reconnect delay in seconds
    #[serde(default = "default_reconnect_delay")]
    pub reconnect_delay_seconds: u64,

    /// Maximum reconnect attempts (0 = unlimited)
    #[serde(default = "default_max_reconnect_attempts")]
    pub max_reconnect_attempts: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct McpSettings {
    /// Enable MCP integration
    #[serde(default)]
    pub enabled: bool,
    
    /// MCP server configurations
    #[serde(default)]
    pub servers: Vec<McpServerSettings>,
    
    /// Timeout for MCP tool calls in seconds
    #[serde(default = "default_mcp_timeout")]
    pub tool_call_timeout_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct McpServerSettings {
    /// Server name/identifier
    pub name: String,
    
    /// Command to run the MCP server
    pub command: String,
    
    /// Arguments for the server command
    #[serde(default)]
    pub args: Vec<String>,
    
    /// Environment variables for the server
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    
    /// Working directory for the server
    pub working_directory: Option<String>,
    
    /// Auto-restart server on failure
    #[serde(default = "default_true")]
    pub auto_restart: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServerSettings {
    /// Enable HTTP server for external agents
    #[serde(default)]
    pub enabled: bool,
    
    /// Server host address
    #[serde(default = "default_server_host")]
    pub host: String,
    
    /// Server port
    #[serde(default = "default_server_port")]
    pub port: u16,
    
    /// Authentication token for API access
    pub auth_token: Option<String>,
    
    /// Enable CORS for browser clients
    #[serde(default = "default_true")]
    pub enable_cors: bool,
    
    /// Request timeout in seconds
    #[serde(default = "default_request_timeout")]
    pub request_timeout_seconds: u64,
}

// Default value functions
fn default_external_url() -> String {
    "localhost:8080".to_string()
}

fn default_true() -> bool {
    true
}

fn default_reconnect_delay() -> u64 {
    5
}

fn default_max_reconnect_attempts() -> u32 {
    10
}

fn default_mcp_timeout() -> u64 {
    30
}

fn default_server_host() -> String {
    "127.0.0.1".to_string()
}

fn default_server_port() -> u16 {
    3030
}

fn default_request_timeout() -> u64 {
    30
}

impl Default for ExternalSyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            websocket_sync: WebSocketSyncSettings::default(),
            mcp: McpSettings::default(),
            server: ServerSettings::default(),
        }
    }
}

impl Default for WebSocketSyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            external_url: default_external_url(),
            auth_token: None,
            use_tls: false,
            skip_tls_verify: false,
            auto_reconnect: default_true(),
            reconnect_delay_seconds: default_reconnect_delay(),
            max_reconnect_attempts: default_max_reconnect_attempts(),
        }
    }
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            servers: Vec::new(),
            tool_call_timeout_seconds: default_mcp_timeout(),
        }
    }
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_server_host(),
            port: default_server_port(),
            auth_token: None,
            enable_cors: default_true(),
            request_timeout_seconds: default_request_timeout(),
        }
    }
}

impl Settings for ExternalSyncSettings {
    fn from_settings(_content: &settings::SettingsContent) -> Self {
        // Load settings from environment variables for containerized deployments
        let enabled = std::env::var("ZED_EXTERNAL_SYNC_ENABLED")
            .map(|v| v.parse().unwrap_or(false))
            .unwrap_or(false);
            
        let websocket_enabled = std::env::var("ZED_WEBSOCKET_SYNC_ENABLED")
            .map(|v| v.parse().unwrap_or(false))
            .unwrap_or(enabled); // Default to same as external sync
            
        let external_url = std::env::var("ZED_HELIX_URL")
            .unwrap_or_else(|_| "localhost:8080".to_string());
            
        let auth_token = std::env::var("ZED_HELIX_TOKEN").ok();
        
        let use_tls = std::env::var("ZED_HELIX_TLS")
            .map(|v| v.parse().unwrap_or(false))
            .unwrap_or(false);

        // Skip TLS verification for enterprise internal CAs (DANGEROUS)
        let skip_tls_verify = std::env::var("ZED_HELIX_SKIP_TLS_VERIFY")
            .map(|v| v.parse().unwrap_or(false))
            .unwrap_or(false);

        Self {
            enabled,
            websocket_sync: WebSocketSyncSettings {
                enabled: websocket_enabled,
                external_url,
                auth_token,
                use_tls,
                skip_tls_verify,
                auto_reconnect: true,
                reconnect_delay_seconds: 5,
                max_reconnect_attempts: 10,
            },
            mcp: McpSettings::default(),
            server: ServerSettings::default(),
        }
    }
}

impl ExternalSyncSettings {
    /// Get the current external sync settings
    pub fn get_global(cx: &App) -> &Self {
        cx.global::<SettingsStore>().get::<Self>(None)
    }
    
    /// Check if any sync method is enabled
    pub fn is_sync_enabled(&self) -> bool {
        self.enabled && (self.websocket_sync.enabled || self.server.enabled)
    }
    
    /// Get the WebSocket URL for connecting to external service
    pub fn websocket_url(&self) -> String {
        let protocol = if self.websocket_sync.use_tls { "wss" } else { "ws" };
        format!("{}://{}/api/v1/external-agents/sync", protocol, self.websocket_sync.external_url)
    }
    
    /// Get server bind address
    pub fn server_bind_address(&self) -> String {
        format!("{}:{}", self.server.host, self.server.port)
    }
}

/// Pre-configured MCP server settings for common tools
impl McpServerSettings {
    /// Create filesystem MCP server configuration
    pub fn filesystem(root_path: String) -> Self {
        Self {
            name: "filesystem".to_string(),
            command: "npx".to_string(),
            args: vec![
                "@modelcontextprotocol/server-filesystem".to_string(),
                root_path,
            ],
            env: std::collections::HashMap::new(),
            working_directory: None,
            auto_restart: true,
        }
    }
    
    /// Create git MCP server configuration
    pub fn git(repo_path: String) -> Self {
        Self {
            name: "git".to_string(),
            command: "npx".to_string(),
            args: vec![
                "@modelcontextprotocol/server-git".to_string(),
                "--repository".to_string(),
                repo_path,
            ],
            env: std::collections::HashMap::new(),
            working_directory: None,
            auto_restart: true,
        }
    }
    
    /// Create SQLite MCP server configuration  
    pub fn sqlite(db_path: String) -> Self {
        Self {
            name: "sqlite".to_string(),
            command: "npx".to_string(),
            args: vec![
                "@modelcontextprotocol/server-sqlite".to_string(),
                db_path,
            ],
            env: std::collections::HashMap::new(),
            working_directory: None,
            auto_restart: true,
        }
    }
    
    /// Create web search MCP server configuration
    pub fn web_search() -> Self {
        Self {
            name: "web_search".to_string(),
            command: "npx".to_string(),
            args: vec![
                "@modelcontextprotocol/server-brave-search".to_string(),
            ],
            env: {
                let mut env = std::collections::HashMap::new();
                env.insert("BRAVE_API_KEY".to_string(), "your_api_key_here".to_string());
                env
            },
            working_directory: None,
            auto_restart: true,
        }
    }
}

/// Initialize external sync settings
pub fn init(cx: &mut App) {
    ExternalSyncSettings::register(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_default_settings() {
        let settings = ExternalSyncSettings::default();
        assert!(!settings.enabled);
        assert!(!settings.websocket_sync.enabled);
        assert!(!settings.mcp.enabled);
        assert!(!settings.server.enabled);
    }
    
    #[test]
    fn test_websocket_url_generation() {
        let mut settings = ExternalSyncSettings::default();
        settings.websocket_sync.external_url = "example.com:8080".to_string();
        settings.websocket_sync.use_tls = false;
        
        assert_eq!(settings.websocket_url(), "ws://example.com:8080/api/v1/external-agents/sync");
        
        settings.websocket_sync.use_tls = true;
        assert_eq!(settings.websocket_url(), "wss://example.com:8080/api/v1/external-agents/sync");
    }
    
    #[test]
    fn test_server_bind_address() {
        let mut settings = ExternalSyncSettings::default();
        settings.server.host = "0.0.0.0".to_string();
        settings.server.port = 4040;
        
        assert_eq!(settings.server_bind_address(), "0.0.0.0:4040");
    }
    
    #[test]
    fn test_mcp_server_presets() {
        let fs_server = McpServerSettings::filesystem("/tmp".to_string());
        assert_eq!(fs_server.name, "filesystem");
        assert_eq!(fs_server.command, "npx");
        assert!(fs_server.args.contains(&"/tmp".to_string()));
        
        let git_server = McpServerSettings::git("/path/to/repo".to_string());
        assert_eq!(git_server.name, "git");
        assert!(git_server.args.contains(&"/path/to/repo".to_string()));
    }
}