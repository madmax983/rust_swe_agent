//! Runtime tool registry and MCP provider support for the default agent loop.
//!
//! `bash` remains the built-in tool. Additional tools come from runtime
//! providers that can be injected programmatically or discovered from MCP
//! servers selected at invocation time.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub const BASH_TOOL_NAME: &str = "bash";
pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const MCP_SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &[MCP_PROTOCOL_VERSION, "2025-06-18", "2025-03-26"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    pub input: String,
}

impl ToolCall {
    pub fn bash(command: impl Into<String>) -> Self {
        Self {
            name: BASH_TOOL_NAME.into(),
            input: command.into(),
        }
    }

    pub fn action_label(&self) -> String {
        if self.name == BASH_TOOL_NAME {
            self.input.clone()
        } else {
            format!("{}:{}", self.name, self.input)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
}

impl ToolDefinition {
    pub fn prompt_info(&self) -> ToolPromptInfo {
        ToolPromptInfo {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self
                .input_schema
                .as_ref()
                .and_then(|schema| serde_json::to_string(schema).ok()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolInvocation {
    pub name: String,
    pub input: String,
    pub task: String,
    pub model: String,
    pub step: u32,
    pub total_cost_usd: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolOutput {
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub timed_out: bool,
}

impl From<ToolOutput> for crate::env::RunResult {
    fn from(output: ToolOutput) -> Self {
        Self {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
            timed_out: output.timed_out,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolPromptInfo {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolsetManifest {
    pub tools: Vec<ToolManifestEntry>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolManifestEntry {
    pub name: String,
    pub description: String,
    pub source: ToolSource,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    BuiltIn,
    CommandAdapter,
    McpServer,
    RuntimeProvider,
}

#[async_trait]
pub trait ToolProvider: Send + Sync {
    fn tools(&self) -> &[ToolDefinition];

    fn source(&self) -> ToolSource {
        ToolSource::RuntimeProvider
    }

    async fn call(
        &self,
        env: &dyn crate::env::Environment,
        invocation: ToolInvocation,
        cancellation: Option<crate::env::CancellationToken>,
    ) -> Result<ToolOutput, crate::error::Error>;
}

#[derive(Debug, Clone)]
pub struct CommandTool {
    pub name: String,
    pub description: Option<String>,
    pub command: String,
    pub timeout_secs: Option<u64>,
}

impl CommandTool {
    pub fn prompt_info(&self) -> ToolPromptInfo {
        ToolPromptInfo {
            name: self.name.clone(),
            description: self
                .description
                .clone()
                .unwrap_or_else(|| "Invocation-time command adapter.".into()),
            input_schema: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    command_tools: BTreeMap<String, CommandTool>,
    provider_tools: BTreeMap<String, ProviderToolEntry>,
    providers: Vec<Arc<dyn ToolProvider>>,
}

#[derive(Clone)]
struct ProviderToolEntry {
    definition: ToolDefinition,
    provider_index: usize,
}

impl ToolRegistry {
    pub fn from_config(tools: &[crate::config::ToolCfg]) -> Self {
        let command_tools = tools
            .iter()
            .map(|tool| {
                (
                    tool.name.clone(),
                    CommandTool {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        command: tool.command.clone(),
                        timeout_secs: tool.timeout_secs,
                    },
                )
            })
            .collect();
        Self {
            command_tools,
            provider_tools: BTreeMap::new(),
            providers: Vec::new(),
        }
    }

    pub fn from_config_and_providers(
        tools: &[crate::config::ToolCfg],
        providers: Vec<Arc<dyn ToolProvider>>,
    ) -> Result<Self, crate::error::Error> {
        let command_tools = tools
            .iter()
            .map(|tool| {
                (
                    tool.name.clone(),
                    CommandTool {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        command: tool.command.clone(),
                        timeout_secs: tool.timeout_secs,
                    },
                )
            })
            .collect();
        let mut registry = Self {
            command_tools,
            provider_tools: BTreeMap::new(),
            providers,
        };
        registry.index_provider_tools()?;
        Ok(registry)
    }

    fn index_provider_tools(&mut self) -> Result<(), crate::error::Error> {
        for (provider_index, provider) in self.providers.iter().enumerate() {
            for definition in provider.tools() {
                validate_tool_name(&definition.name).map_err(|err| {
                    crate::error::Error::Config(crate::error::ConfigError::Invalid(format!(
                        "invalid tool provider name {:?}: {err}",
                        definition.name
                    )))
                })?;
                if definition.name == BASH_TOOL_NAME
                    || self.command_tools.contains_key(&definition.name)
                    || self.provider_tools.contains_key(&definition.name)
                {
                    return Err(crate::error::Error::Config(
                        crate::error::ConfigError::Invalid(format!(
                            "duplicate runtime tool name {:?}",
                            definition.name
                        )),
                    ));
                }
                self.provider_tools.insert(
                    definition.name.clone(),
                    ProviderToolEntry {
                        definition: definition.clone(),
                        provider_index,
                    },
                );
            }
        }
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        name == BASH_TOOL_NAME
            || self.command_tools.contains_key(name)
            || self.provider_tools.contains_key(name)
    }

    pub fn tool_names(&self) -> Vec<String> {
        std::iter::once(BASH_TOOL_NAME.to_owned())
            .chain(self.command_tools.keys().cloned())
            .chain(self.provider_tools.keys().cloned())
            .collect()
    }

    pub fn command_tool(&self, name: &str) -> Option<&CommandTool> {
        self.command_tools.get(name)
    }

    pub fn provider_for(&self, name: &str) -> Option<&dyn ToolProvider> {
        let entry = self.provider_tools.get(name)?;
        self.providers
            .get(entry.provider_index)
            .map(std::convert::AsRef::as_ref)
    }

    pub fn prompt_tools(&self) -> Vec<ToolPromptInfo> {
        std::iter::once(ToolPromptInfo {
            name: BASH_TOOL_NAME.into(),
            description: "Run shell commands in the configured environment.".into(),
            input_schema: None,
        })
        .chain(self.command_tools.values().map(CommandTool::prompt_info))
        .chain(
            self.provider_tools
                .values()
                .map(|entry| entry.definition.prompt_info()),
        )
        .collect()
    }

    pub fn manifest(&self) -> ToolsetManifest {
        let tools = std::iter::once(ToolManifestEntry {
            name: BASH_TOOL_NAME.into(),
            description: "Run shell commands in the configured environment.".into(),
            source: ToolSource::BuiltIn,
        })
        .chain(self.command_tools.values().map(|tool| {
            ToolManifestEntry {
                name: tool.name.clone(),
                description: tool
                    .description
                    .clone()
                    .unwrap_or_else(|| "Invocation-time command adapter.".into()),
                source: ToolSource::CommandAdapter,
            }
        }))
        .chain(self.provider_tools.values().map(|entry| ToolManifestEntry {
            name: entry.definition.name.clone(),
            description: entry.definition.description.clone(),
            source: self.providers[entry.provider_index].source(),
        }))
        .collect();
        ToolsetManifest { tools }
    }
}

pub struct McpStdioServer {
    command: String,
    timeout: Duration,
    protocol_version: String,
    tools: Vec<ToolDefinition>,
}

impl McpStdioServer {
    pub async fn discover(
        env: &dyn crate::env::Environment,
        cfg: &crate::config::McpServerCfg,
        default_timeout_secs: u64,
        cancellation: Option<crate::env::CancellationToken>,
    ) -> Result<Self, crate::error::Error> {
        let timeout = Duration::from_secs(cfg.timeout_secs.unwrap_or(default_timeout_secs));
        let mut cursor = None;
        let mut protocol_version = MCP_PROTOCOL_VERSION.to_owned();
        let mut tools = Vec::new();

        loop {
            let (initialize_result, list_result) = run_mcp_exchange(
                env,
                &cfg.command,
                timeout,
                mcp_tools_list_messages(cursor.clone(), &protocol_version),
                2,
                cancellation.clone(),
            )
            .await?;
            let negotiated = parse_mcp_initialize_protocol(&cfg.command, initialize_result)?;
            if cursor.is_some() && negotiated != protocol_version {
                return Err(crate::error::Error::Config(
                    crate::error::ConfigError::Invalid(format!(
                        "MCP server `{}` changed negotiated protocol version from `{}` to `{}` during tools/list pagination",
                        cfg.command, protocol_version, negotiated
                    )),
                ));
            }
            protocol_version = negotiated;

            let page = parse_mcp_tools_list(&cfg.command, list_result)?;
            tools.extend(page.tools);
            cursor = page.next_cursor.filter(|cursor| !cursor.is_empty());
            if cursor.is_none() {
                break;
            }
        }

        Ok(Self {
            command: cfg.command.clone(),
            timeout,
            protocol_version,
            tools,
        })
    }
}

#[async_trait]
impl ToolProvider for McpStdioServer {
    fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    fn source(&self) -> ToolSource {
        ToolSource::McpServer
    }

    async fn call(
        &self,
        env: &dyn crate::env::Environment,
        invocation: ToolInvocation,
        cancellation: Option<crate::env::CancellationToken>,
    ) -> Result<ToolOutput, crate::error::Error> {
        let arguments = tool_input_to_mcp_arguments(&invocation.input);
        let (initialize_result, call_result) = run_mcp_exchange(
            env,
            &self.command,
            self.timeout,
            mcp_tools_call_messages(&invocation.name, &arguments, &self.protocol_version),
            2,
            cancellation,
        )
        .await?;
        let protocol_version = parse_mcp_initialize_protocol(&self.command, initialize_result)?;
        if protocol_version != self.protocol_version {
            return Err(crate::error::Error::Config(
                crate::error::ConfigError::Invalid(format!(
                    "MCP server `{}` changed negotiated protocol version from `{}` to `{}`",
                    self.command, self.protocol_version, protocol_version
                )),
            ));
        }
        parse_mcp_tool_output(&self.command, &call_result)
    }
}

#[derive(Debug, Deserialize)]
struct McpToolsListResult {
    tools: Vec<McpToolDefinition>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}

struct ParsedMcpToolsList {
    tools: Vec<ToolDefinition>,
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpInitializeResult {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
}

#[derive(Debug, Deserialize)]
struct McpToolDefinition {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct McpJsonRpcResponse {
    id: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<McpJsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct McpJsonRpcError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

fn mcp_tools_list_messages(
    cursor: Option<String>,
    protocol_version: &str,
) -> Vec<serde_json::Value> {
    let params = cursor.map_or_else(
        || serde_json::json!({}),
        |cursor| serde_json::json!({ "cursor": cursor }),
    );
    vec![
        mcp_initialize_request(1, protocol_version),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": params,
        }),
    ]
}

fn mcp_tools_call_messages(
    name: &str,
    arguments: &serde_json::Value,
    protocol_version: &str,
) -> Vec<serde_json::Value> {
    vec![
        mcp_initialize_request(1, protocol_version),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments,
            },
        }),
    ]
}

fn mcp_initialize_request(id: u64, protocol_version: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": protocol_version,
            "capabilities": {},
            "clientInfo": {
                "name": "rust-swe-agent",
                "version": env!("CARGO_PKG_VERSION"),
            },
        },
    })
}

fn parse_mcp_initialize_protocol(
    command: &str,
    value: serde_json::Value,
) -> Result<String, crate::error::Error> {
    let result: McpInitializeResult = serde_json::from_value(value).map_err(|err| {
        crate::error::Error::Config(crate::error::ConfigError::Invalid(format!(
            "MCP server `{command}` returned invalid initialize result: {err}"
        )))
    })?;
    if MCP_SUPPORTED_PROTOCOL_VERSIONS.contains(&result.protocol_version.as_str()) {
        Ok(result.protocol_version)
    } else {
        Err(crate::error::Error::Config(
            crate::error::ConfigError::Invalid(format!(
                "MCP server `{command}` negotiated unsupported MCP protocol version `{}`; supported versions: {}",
                result.protocol_version,
                MCP_SUPPORTED_PROTOCOL_VERSIONS.join(", ")
            )),
        ))
    }
}

async fn run_mcp_exchange(
    env: &dyn crate::env::Environment,
    command: &str,
    timeout: Duration,
    messages: Vec<serde_json::Value>,
    response_id: u64,
    cancellation: Option<crate::env::CancellationToken>,
) -> Result<(serde_json::Value, serde_json::Value), crate::error::Error> {
    let stdin = messages
        .into_iter()
        .map(|message| serde_json::to_string(&message))
        .collect::<Result<Vec<_>, _>>()?
        .join("\n")
        + "\n";
    let mut run_req = crate::env::RunRequest::new(command)
        .with_timeout(timeout)
        .with_stdin(stdin);
    if let Some(cancellation) = cancellation {
        run_req = run_req.with_cancellation(cancellation);
    }
    let result = env.run(run_req).await?;
    if result.timed_out || result.exit_code != 0 {
        return Err(crate::error::Error::Config(
            crate::error::ConfigError::Invalid(format!(
                "MCP server `{command}` failed during protocol exchange: exit_code={} timed_out={} stderr={}",
                result.exit_code,
                result.timed_out,
                result.stderr.trim()
            )),
        ));
    }
    let initialize = mcp_response_result(command, &result.stdout, 1)?;
    let response = mcp_response_result(command, &result.stdout, response_id)?;
    Ok((initialize, response))
}

fn mcp_response_result(
    command: &str,
    stdout: &str,
    wanted_id: u64,
) -> Result<serde_json::Value, crate::error::Error> {
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(response) = serde_json::from_str::<McpJsonRpcResponse>(line) else {
            continue;
        };
        if !json_id_matches(response.id.as_ref(), wanted_id) {
            continue;
        }
        if let Some(error) = response.error {
            let data = error
                .data
                .map_or_else(String::new, |data| format!(" data={data}"));
            return Err(crate::error::Error::Config(
                crate::error::ConfigError::Invalid(format!(
                    "MCP server `{command}` returned JSON-RPC error {}: {}{}",
                    error.code, error.message, data
                )),
            ));
        }
        return response.result.ok_or_else(|| {
            crate::error::Error::Config(crate::error::ConfigError::Invalid(format!(
                "MCP server `{command}` response id {wanted_id} omitted result"
            )))
        });
    }
    Err(crate::error::Error::Config(
        crate::error::ConfigError::Invalid(format!(
            "MCP server `{command}` did not return response id {wanted_id}"
        )),
    ))
}

fn json_id_matches(id: Option<&serde_json::Value>, wanted_id: u64) -> bool {
    id.and_then(serde_json::Value::as_u64) == Some(wanted_id)
}

fn parse_mcp_tools_list(
    command: &str,
    value: serde_json::Value,
) -> Result<ParsedMcpToolsList, crate::error::Error> {
    let result: McpToolsListResult = serde_json::from_value(value).map_err(|err| {
        crate::error::Error::Config(crate::error::ConfigError::Invalid(format!(
            "MCP server `{command}` returned invalid tools/list result: {err}"
        )))
    })?;
    let tools = result
        .tools
        .into_iter()
        .map(|tool| ToolDefinition {
            name: tool.name,
            description: tool.description.or(tool.title).unwrap_or_default(),
            input_schema: tool.input_schema,
        })
        .collect();
    Ok(ParsedMcpToolsList {
        tools,
        next_cursor: result.next_cursor,
    })
}

fn tool_input_to_mcp_arguments(input: &str) -> serde_json::Value {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        Ok(value) => serde_json::json!({ "input": value }),
        Err(_) => serde_json::json!({ "input": input }),
    }
}

fn parse_mcp_tool_output(
    command: &str,
    value: &serde_json::Value,
) -> Result<ToolOutput, crate::error::Error> {
    let content = value
        .get("content")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            crate::error::Error::Config(crate::error::ConfigError::Invalid(format!(
                "MCP server `{command}` returned tools/call result without content array"
            )))
        })?;
    let stdout = content
        .iter()
        .map(mcp_content_to_text)
        .collect::<Vec<_>>()
        .join("\n");
    let is_error = value
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(ToolOutput {
        stdout,
        stderr: String::new(),
        exit_code: i32::from(is_error),
        timed_out: false,
    })
}

fn mcp_content_to_text(content: &serde_json::Value) -> String {
    if content.get("type").and_then(serde_json::Value::as_str) == Some("text") {
        content
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    } else {
        content.to_string()
    }
}

pub async fn discover_mcp_servers(
    env: &dyn crate::env::Environment,
    servers: &[crate::config::McpServerCfg],
    default_timeout_secs: u64,
    cancellation: Option<crate::env::CancellationToken>,
) -> Result<Vec<Arc<dyn ToolProvider>>, crate::error::Error> {
    let mut providers: Vec<Arc<dyn ToolProvider>> = Vec::new();
    for server in servers {
        let provider =
            McpStdioServer::discover(env, server, default_timeout_secs, cancellation.clone())
                .await?;
        providers.push(Arc::new(provider));
    }
    Ok(providers)
}

pub fn validate_tool_name(name: &str) -> Result<(), &'static str> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("tool name cannot be empty");
    };
    if !first.is_ascii_alphabetic() {
        return Err("tool name must start with an ASCII letter");
    }
    if chars.any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')) {
        return Err("tool name may contain only ASCII letters, digits, `_`, or `-`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::env::{Environment, RunRequest, RunResult};
    use crate::error::EnvError;

    #[tokio::test]
    async fn mcp_stdio_server_discovers_and_calls_tools_via_json_rpc() {
        let env = JsonPluginEnv::default();
        let cfg = crate::config::McpServerCfg {
            command: "diagnostic-mcp".into(),
            timeout_secs: Some(3),
        };

        let server = McpStdioServer::discover(&env, &cfg, 10, None)
            .await
            .unwrap();
        assert_eq!(server.tools()[0].name, "diagnose");

        let output = server
            .call(
                &env,
                ToolInvocation {
                    name: "diagnose".into(),
                    input: "{\"query\":\"check flaky test\"}".into(),
                    task: "fix it".into(),
                    model: "deterministic".into(),
                    step: 2,
                    total_cost_usd: 0.25,
                },
                None,
            )
            .await
            .unwrap();

        assert_eq!(output.stdout, "mcp saw check flaky test");
        let requests = env.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].command, "diagnostic-mcp");
        assert!(requests[0].stdin.as_deref().unwrap().contains("initialize"));
        assert!(
            requests[0]
                .stdin
                .as_deref()
                .unwrap()
                .contains("\"protocolVersion\":\"2025-11-25\"")
        );
        assert!(requests[0].stdin.as_deref().unwrap().contains("tools/list"));
        assert!(requests[1].stdin.as_deref().unwrap().contains("tools/call"));
        assert!(
            requests[1]
                .stdin
                .as_deref()
                .unwrap()
                .contains("\"arguments\":{\"query\":\"check flaky test\"}")
        );
    }

    #[tokio::test]
    async fn mcp_stdio_server_uses_negotiated_protocol_for_tool_calls() {
        let env = JsonPluginEnv::with_protocol_version("2025-03-26");
        let cfg = crate::config::McpServerCfg {
            command: "diagnostic-mcp".into(),
            timeout_secs: Some(3),
        };

        let server = McpStdioServer::discover(&env, &cfg, 10, None)
            .await
            .unwrap();
        server
            .call(
                &env,
                ToolInvocation {
                    name: "diagnose".into(),
                    input: "{\"query\":\"check flaky test\"}".into(),
                    task: "fix it".into(),
                    model: "deterministic".into(),
                    step: 2,
                    total_cost_usd: 0.25,
                },
                None,
            )
            .await
            .unwrap();

        let requests = env.requests.lock().unwrap().clone();
        assert!(
            requests[0]
                .stdin
                .as_deref()
                .unwrap()
                .contains("\"protocolVersion\":\"2025-11-25\""),
            "initial discovery should advertise the latest supported MCP revision"
        );
        assert!(
            requests[1]
                .stdin
                .as_deref()
                .unwrap()
                .contains("\"protocolVersion\":\"2025-03-26\""),
            "tool calls should reuse the protocol version negotiated during discovery"
        );
    }

    #[tokio::test]
    async fn mcp_stdio_server_rejects_unsupported_negotiated_protocol() {
        let env = JsonPluginEnv::with_protocol_version("1900-01-01");
        let cfg = crate::config::McpServerCfg {
            command: "diagnostic-mcp".into(),
            timeout_secs: Some(3),
        };

        let Err(err) = McpStdioServer::discover(&env, &cfg, 10, None).await else {
            panic!("expected unsupported protocol version to fail discovery");
        };
        assert!(
            err.to_string()
                .contains("unsupported MCP protocol version `1900-01-01`"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn mcp_stdio_server_rejects_protocol_change_during_tool_call() {
        let env = JsonPluginEnv::with_protocol_versions(["2025-06-18", "2025-03-26"]);
        let cfg = crate::config::McpServerCfg {
            command: "diagnostic-mcp".into(),
            timeout_secs: Some(3),
        };

        let server = McpStdioServer::discover(&env, &cfg, 10, None)
            .await
            .unwrap();
        let err = server
            .call(
                &env,
                ToolInvocation {
                    name: "diagnose".into(),
                    input: "{\"query\":\"check flaky test\"}".into(),
                    task: "fix it".into(),
                    model: "deterministic".into(),
                    step: 2,
                    total_cost_usd: 0.25,
                },
                None,
            )
            .await
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("changed negotiated protocol version from `2025-06-18` to `2025-03-26`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn mcp_response_result_skips_stdout_noise_before_json_rpc() {
        let stdout = format!(
            "diagnostic banner\nnot json\n{}\n",
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "serverInfo": {"name": "diagnostic-mcp", "version": "1.0.0"}
                }
            })
        );

        let result = mcp_response_result("diagnostic-mcp", &stdout, 1).unwrap();
        assert_eq!(
            result["protocolVersion"].as_str(),
            Some(MCP_PROTOCOL_VERSION)
        );
    }

    #[tokio::test]
    async fn mcp_stdio_server_discovers_paginated_tools_list() {
        let env = PaginatedMcpEnv::default();
        let cfg = crate::config::McpServerCfg {
            command: "diagnostic-mcp".into(),
            timeout_secs: Some(3),
        };

        let server = McpStdioServer::discover(&env, &cfg, 10, None)
            .await
            .unwrap();
        let names = server
            .tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["diagnose", "repair"]);

        let requests = env.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .stdin
                .as_deref()
                .unwrap()
                .contains("\"cursor\":\"page-2\""),
            "second tools/list request should include nextCursor: {requests:#?}"
        );
    }

    struct JsonPluginEnv {
        requests: Arc<Mutex<Vec<RunRequest>>>,
        protocol_versions: Arc<Mutex<Vec<String>>>,
    }

    impl JsonPluginEnv {
        fn with_protocol_version(protocol_version: impl Into<String>) -> Self {
            Self::with_protocol_versions([protocol_version])
        }

        fn with_protocol_versions(
            protocol_versions: impl IntoIterator<Item = impl Into<String>>,
        ) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                protocol_versions: Arc::new(Mutex::new(
                    protocol_versions.into_iter().map(Into::into).collect(),
                )),
            }
        }

        fn next_protocol_version(&self) -> String {
            let mut versions = self.protocol_versions.lock().unwrap();
            let version = if versions.len() > 1 {
                versions.remove(0)
            } else {
                let Some(version) = versions.first().cloned() else {
                    panic!("test MCP env must have at least one protocol version");
                };
                version
            };
            drop(versions);
            version
        }
    }

    impl Default for JsonPluginEnv {
        fn default() -> Self {
            Self::with_protocol_version("2025-11-25")
        }
    }

    #[derive(Default)]
    struct PaginatedMcpEnv {
        requests: Arc<Mutex<Vec<RunRequest>>>,
    }

    #[async_trait]
    impl Environment for JsonPluginEnv {
        async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
            self.requests.lock().unwrap().push(req.clone());
            let stdin = req.stdin.unwrap_or_default();
            let mut responses = Vec::new();
            for line in stdin.lines() {
                let request: serde_json::Value = serde_json::from_str(line).unwrap();
                let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                match method {
                    "initialize" => responses.push(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {
                            "protocolVersion": self.next_protocol_version(),
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "diagnostic-mcp", "version": "1.0.0"},
                        },
                    })),
                    "notifications/initialized" => {}
                    "tools/list" => responses.push(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {
                            "tools": [{
                                "name": "diagnose",
                                "description": "Run diagnostics.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "query": {"type": "string"}
                                    },
                                    "required": ["query"]
                                },
                            }]
                        },
                    })),
                    "tools/call" => {
                        let query = request["params"]["arguments"]["query"]
                            .as_str()
                            .unwrap_or_default();
                        responses.push(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request["id"],
                            "result": {
                                "content": [{"type": "text", "text": format!("mcp saw {query}")}],
                                "isError": false,
                            },
                        }));
                    }
                    other => panic!("unexpected MCP request method: {other}"),
                }
            }
            let stdout = responses
                .into_iter()
                .map(|response| serde_json::to_string(&response).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            Ok(RunResult {
                stdout,
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            })
        }
    }

    #[async_trait]
    impl Environment for PaginatedMcpEnv {
        async fn run(&self, req: RunRequest) -> Result<RunResult, EnvError> {
            self.requests.lock().unwrap().push(req.clone());
            let stdin = req.stdin.unwrap_or_default();
            let mut responses = Vec::new();
            for line in stdin.lines() {
                let request: serde_json::Value = serde_json::from_str(line).unwrap();
                let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                match method {
                    "initialize" => responses.push(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {
                            "protocolVersion": MCP_PROTOCOL_VERSION,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "diagnostic-mcp", "version": "1.0.0"},
                        },
                    })),
                    "notifications/initialized" => {}
                    "tools/list" => {
                        let cursor = request["params"]["cursor"].as_str();
                        let result = match cursor {
                            None => serde_json::json!({
                                "tools": [{
                                    "name": "diagnose",
                                    "description": "Run diagnostics."
                                }],
                                "nextCursor": "page-2"
                            }),
                            Some("page-2") => serde_json::json!({
                                "tools": [{
                                    "name": "repair",
                                    "description": "Run repair."
                                }]
                            }),
                            other => panic!("unexpected tools/list cursor: {other:?}"),
                        };
                        responses.push(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request["id"],
                            "result": result,
                        }));
                    }
                    other => panic!("unexpected MCP request method: {other}"),
                }
            }
            let stdout = responses
                .into_iter()
                .map(|response| serde_json::to_string(&response).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            Ok(RunResult {
                stdout,
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            })
        }
    }
}
