//! CLI fallback: `hoo-browser cli <agent-browser args...>`.
//!
//! Connects to the live helper over its Unix socket (from
//! `HOOCODE_HELPER_SOCKET_HOO_BROWSER` with `HOOCODE_AGENT_TOKEN`) and calls
//! the hidden `browser_cli {args}` tool, so agent-browser runs scoped to the
//! caller's own sessions. Prints agent-browser's output.

use rmcp::model::{CallToolRequestParams, RequestMetaObject};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::{Map, Value};

use crate::error::HooError;

/// Socket env set by hoocode's Shell for every live tier C helper.
const SOCKET_ENV: &str = "HOOCODE_HELPER_SOCKET_HOO_BROWSER";
const TOKEN_ENV: &str = "HOOCODE_AGENT_TOKEN";

pub async fn run_cli_fallback(argv: &[String]) -> Result<(), HooError> {
    let socket = std::env::var(SOCKET_ENV).map_err(|_| {
        HooError::Usage(format!(
            "{SOCKET_ENV} is not set: no live hoo-browser helper (is the plugin installed?)"
        ))
    })?;
    let token = std::env::var(TOKEN_ENV).map_err(|_| {
        HooError::Usage(format!(
            "{TOKEN_ENV} is not set: the CLI fallback runs inside an agent Shell"
        ))
    })?;
    if argv.is_empty() {
        return Err(HooError::Usage(
            "usage: hoo-browser cli <agent-browser args...>".into(),
        ));
    }
    let mut config = StreamableHttpClientTransportConfig::with_uri("http://localhost/mcp");
    // The transport adds the `Bearer ` prefix itself.
    config.auth_header = Some(token);
    let transport = StreamableHttpClientTransport::from_unix_socket_with_config(&socket, config);
    let peer = ()
        .serve(transport)
        .await
        .map_err(|e| HooError::Transport(format!("cannot reach the hoo-browser helper: {e}")))?;

    let mut params = CallToolRequestParams::new("browser_cli".to_owned());
    let mut arguments = Map::new();
    arguments.insert(
        "args".to_owned(),
        Value::Array(argv.iter().map(|a| Value::String(a.clone())).collect()),
    );
    params.arguments = Some(arguments);
    // Agent identity for the helper's logs, mirroring hoocode's own client.
    let mut meta = RequestMetaObject::new();
    meta.insert(
        "dev.hoocode/agent".to_owned(),
        serde_json::json!({
            "agentId": std::env::var("HOOCODE_AGENT_ID").unwrap_or_else(|_| "unknown".into()),
            "parentId": std::env::var("HOOCODE_AGENT_PARENT").ok(),
            "depth": std::env::var("HOOCODE_AGENT_DEPTH").ok().and_then(|d| d.parse::<u64>().ok()).unwrap_or(0),
        }),
    );
    params.meta = Some(meta);

    let result = peer
        .call_tool_once(params)
        .await
        .map_err(|e| HooError::Transport(format!("browser_cli failed: {e}")))?;
    match result {
        rmcp::model::CallToolResponse::Complete(result) => {
            let mut failed = result.is_error.unwrap_or(false);
            for block in &result.content {
                match block {
                    rmcp::model::ContentBlock::Text(text) => {
                        print!("{}", text.text);
                        if !text.text.ends_with('\n') {
                            println!();
                        }
                    }
                    other => {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(other)
                                .unwrap_or_else(|_| "(unprintable content)".into())
                        );
                        if !matches!(other, rmcp::model::ContentBlock::Image(_)) {
                            failed = true;
                        }
                    }
                }
            }
            if failed {
                std::process::exit(1);
            }
            Ok(())
        }
        _ => Err(HooError::Transport(
            "browser_cli returned an unexpected response".into(),
        )),
    }
}
