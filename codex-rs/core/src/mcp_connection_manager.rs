//! Connection manager for Model Context Protocol (MCP) servers.
//!
//! The [`McpConnectionManager`] owns one [`codex_mcp_client::McpClient`] per
//! configured server (keyed by the *server name*). It offers convenience
//! helpers to query the available tools across *all* servers and returns them
//! in a single aggregated map using the fully-qualified tool name
//! `"<server><MCP_TOOL_NAME_DELIMITER><tool>"` as the key.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use codex_mcp_client::McpClient;
use mcp_types::ClientCapabilities;
use mcp_types::Implementation;
use mcp_types::Tool;
use regex_lite::Regex;
use tokio::task::JoinSet;
use tracing::info;

use crate::config_types::McpServerConfig;

/// Delimiter used to separate the server name from the tool name in a fully
/// qualified tool name.
///
/// OpenAI requires tool names to conform to `^[a-zA-Z0-9_-]+$`, so we must
/// choose a delimiter from this character set.
const MCP_TOOL_NAME_DELIMITER: &str = "__OAI_CODEX_MCP__";

/// Regular expression that valid server and tool names must match.
static VALID_NAME_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]+$").expect("compile name validation regex"));

/// Timeout for the `tools/list` request.
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(10);

/// Map that holds a startup error for every MCP server that could **not** be
/// spawned successfully.
pub type ClientStartErrors = HashMap<String, anyhow::Error>;

fn fully_qualified_tool_name(server: &str, tool: &str) -> String {
    format!("{server}{MCP_TOOL_NAME_DELIMITER}{tool}")
}

pub(crate) fn try_parse_fully_qualified_tool_name(fq_name: &str) -> Option<(String, String)> {
    let (server, tool) = fq_name.split_once(MCP_TOOL_NAME_DELIMITER)?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server.to_string(), tool.to_string()))
}

fn valid_name(name: &str) -> bool {
    VALID_NAME_REGEX.is_match(name)
}

/// A thin wrapper around a set of running [`McpClient`] instances.
#[derive(Default)]
pub(crate) struct McpConnectionManager {
    /// Server-name -> client instance.
    ///
    /// The server name originates from the keys of the `mcp_servers` map in
    /// the user configuration.
    clients: HashMap<String, std::sync::Arc<McpClient>>,

    /// Fully qualified tool name -> tool instance.
    tools: HashMap<String, Tool>,
}

impl McpConnectionManager {
    /// Spawn a [`McpClient`] for each configured server.
    ///
    /// * `mcp_servers` – Map loaded from the user configuration where *keys*
    ///   are human-readable server identifiers and *values* are the spawn
    ///   instructions.
    ///
    /// Servers that fail to start are reported in `ClientStartErrors`: the
    /// user should be informed about these errors.
    pub async fn new(
        mcp_servers: HashMap<String, McpServerConfig>,
    ) -> Result<(Self, ClientStartErrors)> {
        // Early exit if no servers are configured.
        if mcp_servers.is_empty() {
            return Ok((Self::default(), ClientStartErrors::default()));
        }

        // Launch all configured servers concurrently.
        let mut join_set = JoinSet::new();
        let mut errors = ClientStartErrors::new();

        for (server_name, cfg) in mcp_servers {
            if !valid_name(&server_name) {
                errors.insert(
                    server_name,
                    anyhow!("invalid server name; must match ^[a-zA-Z0-9_-]+$"),
                );
                continue;
            }
            join_set.spawn(async move {
                let McpServerConfig { command, args, env } = cfg;
                let client_res = McpClient::new_stdio_client(command, args, env).await;
                match client_res {
                    Ok(client) => {
                        // Initialize the client.
                        let params = mcp_types::InitializeRequestParams {
                            capabilities: ClientCapabilities {
                                experimental: None,
                                roots: None,
                                sampling: None,
                            },
                            client_info: Implementation {
                                name: "codex-mcp-client".to_owned(),
                                version: env!("CARGO_PKG_VERSION").to_owned(),
                            },
                            protocol_version: mcp_types::MCP_SCHEMA_VERSION.to_owned(),
                        };
                        let initialize_notification_params = None;
                        let timeout = Some(Duration::from_secs(10));
                        match client
                            .initialize(params, initialize_notification_params, timeout)
                            .await
                        {
                            Ok(_response) => (server_name, Ok(client)),
                            Err(e) => (server_name, Err(e)),
                        }
                    }
                    Err(e) => (server_name, Err(e.into())),
                }
            });
        }

        let mut clients: HashMap<String, std::sync::Arc<McpClient>> =
            HashMap::with_capacity(join_set.len());

        while let Some(res) = join_set.join_next().await {
            let (server_name, client_res) = res?; // JoinError propagation

            match client_res {
                Ok(client) => {
                    clients.insert(server_name, std::sync::Arc::new(client));
                }
                Err(e) => {
                    errors.insert(server_name, e);
                }
            }
        }

        let tools = list_all_tools(&clients).await?;

        Ok((Self { clients, tools }, errors))
    }

    /// Returns a single map that contains **all** tools. Each key is the
    /// fully-qualified name for the tool.
    pub fn list_all_tools(&self) -> HashMap<String, Tool> {
        self.tools.clone()
    }

    /// Invoke the tool indicated by the (server, tool) pair.
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Value>,
        timeout: Option<Duration>,
    ) -> Result<mcp_types::CallToolResult> {
        let client = self
            .clients
            .get(server)
            .ok_or_else(|| anyhow!("unknown MCP server '{server}'"))?
            .clone();

        client
            .call_tool(tool.to_string(), arguments, timeout)
            .await
            .with_context(|| format!("tool call failed for `{server}/{tool}`"))
    }
}

/// Query every server for its available tools and return a single map that
/// contains **all** tools. Each key is the fully-qualified name for the tool.
pub async fn list_all_tools(
    clients: &HashMap<String, std::sync::Arc<McpClient>>,
) -> Result<HashMap<String, Tool>> {
    let mut join_set = JoinSet::new();

    // Spawn one task per server so we can query them concurrently. This
    // keeps the overall latency roughly at the slowest server instead of
    // the cumulative latency.
    for (server_name, client) in clients {
        let server_name_cloned = server_name.clone();
        let client_clone = client.clone();
        join_set.spawn(async move {
            let res = client_clone
                .list_tools(None, Some(LIST_TOOLS_TIMEOUT))
                .await;
            (server_name_cloned, res)
        });
    }

    let mut aggregated: HashMap<String, Tool> = HashMap::with_capacity(join_set.len());

    while let Some(join_res) = join_set.join_next().await {
        let (server_name, list_result) = join_res?;
        let list_result = list_result?;

        for tool in list_result.tools {
            if !valid_name(&tool.name) {
                info!(
                    "ignoring invalid tool name '{}' from server '{}'",
                    tool.name, server_name
                );
                continue;
            }
            let fq_name = fully_qualified_tool_name(&server_name, &tool.name);
            if aggregated.insert(fq_name.clone(), tool).is_some() {
                panic!("tool name collision for '{fq_name}': suspicious");
            }
        }
    }

    info!(
        "aggregated {} tools from {} servers",
        aggregated.len(),
        clients.len()
    );

    Ok(aggregated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[tokio::test]
    async fn reject_invalid_server_name() {
        let mut servers = HashMap::new();
        servers.insert(
            "bad name".to_string(),
            McpServerConfig {
                command: "true".into(),
                args: vec![],
                env: None,
            },
        );

        let (mgr, errors) = McpConnectionManager::new(servers).await.unwrap();
        assert!(mgr.list_all_tools().is_empty());
        assert!(errors.contains_key("bad name"));
    }

    #[tokio::test]
    async fn invalid_tool_name_filtered() {
        // Create temp script that acts as a minimal MCP server returning an invalid tool name
        let dir = TempDir::new().unwrap();
        let script_path = dir.path().join("server.js");
        let mut f = File::create(&script_path).unwrap();
        let script = format!(
            "const rl=require('readline').createInterface({{input:process.stdin}});\nrl.on('line',l=>{{let m=JSON.parse(l);if(m.method==='initialize'){{console.log(JSON.stringify({{jsonrpc:'2.0',id:m.id,result:{{capabilities:{{}},protocolVersion:'{}',serverInfo:{{name:'test',version:'0'}}}}}}));}}else if(m.method==='notifications/initialized'){{}}else if(m.method==='tools/list'){{console.log(JSON.stringify({{jsonrpc:'2.0',id:m.id,result:{{tools:[{{name:'bad tool!',inputSchema:{{type:'object'}}}}],next_cursor:null}}}}));}}}});",
            mcp_types::MCP_SCHEMA_VERSION
        );
        f.write_all(script.as_bytes()).unwrap();
        drop(f);

        let mut servers = HashMap::new();
        servers.insert(
            "srv".to_string(),
            McpServerConfig {
                command: "node".into(),
                args: vec![script_path.to_string_lossy().into()],
                env: None,
            },
        );

        let (mgr, errors) = McpConnectionManager::new(servers).await.unwrap();
        println!("errors: {:?}", errors);
        assert!(errors.is_empty());
        let tools = mgr.list_all_tools();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    #[should_panic]
    async fn duplicate_tool_name_panics() {
        // Server returns the same tool twice.
        let dir = TempDir::new().unwrap();
        let script_path = dir.path().join("server.js");
        let mut f = File::create(&script_path).unwrap();
        let script = format!(
            "const rl=require('readline').createInterface({{input:process.stdin}});\nrl.on('line',l=>{{let m=JSON.parse(l);if(m.method==='initialize'){{console.log(JSON.stringify({{jsonrpc:'2.0',id:m.id,result:{{capabilities:{{}},protocolVersion:'{}',serverInfo:{{name:'test',version:'0'}}}}}}));}}else if(m.method==='notifications/initialized'){{}}else if(m.method==='tools/list'){{console.log(JSON.stringify({{jsonrpc:'2.0',id:m.id,result:{{tools:[{{name:'dup',inputSchema:{{type:'object'}}}},{{name:'dup',inputSchema:{{type:'object'}}}}],next_cursor:null}}}}));}}}});",
            mcp_types::MCP_SCHEMA_VERSION
        );
        f.write_all(script.as_bytes()).unwrap();
        drop(f);

        let mut servers = HashMap::new();
        servers.insert(
            "srv".to_string(),
            McpServerConfig {
                command: "node".into(),
                args: vec![script_path.to_string_lossy().into()],
                env: None,
            },
        );

        let (_mgr, errors) = McpConnectionManager::new(servers).await.unwrap();
        println!("errors: {:?}", errors);
    }
}
