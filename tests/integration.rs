//! Integration tests: the helper as a child of the test, driven by an MCP
//! client over the Unix socket.
//!
//! Two backends, one harness:
//! - Fake (always runs): `tests/fake-agent-browser.sh` answers canned `--json`
//!   envelopes, so leases, auth, registration rules, arg mapping and the
//!   screenshot-to-image path are covered with no Chromium.
//! - Real (`HOO_BROWSER_IT=1`): the real agent-browser CLI plus a headless
//!   Chromium the test launches itself, against a local test page.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hoo_browser::server::Helper;
use rmcp::model::{CallToolRequestParams, CallToolResponse, ContentBlock, CustomNotification};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ROOT_TOKEN: &str = "root-token-for-tests";

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn test_tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hoo-browser-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

async fn start_helper(agent_browser: &Path, data_dir: &Path) -> (Helper, PathBuf) {
    let socket = data_dir.join("helper.sock");
    let helper = Helper::new(
        agent_browser.to_owned(),
        "ws://fake-endpoint-for-tests".into(),
        data_dir.to_owned(),
        hoo_browser::backend::socket_dir(data_dir),
        ROOT_TOKEN.into(),
        "root".into(),
    );
    let _ = std::fs::remove_file(&socket);
    tokio::spawn(hoo_browser::server::serve(helper.clone(), socket.clone()));
    wait_for_socket(&socket).await;
    (helper, socket)
}

async fn wait_for_socket(socket: &Path) {
    for _ in 0..100 {
        if socket.exists() {
            // The socket file appears before `initialize` answers; one probe
            // connect tells us the server is up.
            if tokio::net::UnixStream::connect(socket).await.is_ok() {
                // Give the accept loop a moment to settle.
                tokio::time::sleep(Duration::from_millis(20)).await;
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("helper socket never appeared: {}", socket.display());
}

type TestPeer = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn try_connect(socket: &Path, token: &str) -> Result<TestPeer, String> {
    let mut config = StreamableHttpClientTransportConfig::with_uri("http://localhost/mcp");
    // The transport adds the `Bearer ` prefix itself.
    config.auth_header = Some(token.to_owned());
    let transport = StreamableHttpClientTransport::from_unix_socket_with_config(
        &socket.to_string_lossy(),
        config,
    );
    ().serve(transport).await.map_err(|e| format!("{e}"))
}

async fn connect(socket: &Path, token: &str) -> TestPeer {
    try_connect(socket, token)
        .await
        .unwrap_or_else(|e| panic!("MCP handshake failed: {e}"))
}

/// Poll the helper's token map instead of sleeping a fixed delay after
/// a notification: notifications are handled asynchronously, so a sleep
/// either flakes or wastes time.
async fn wait_registered(helper: &Helper, token: &str) {
    for _ in 0..150 {
        if helper.agent_for(token).await.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("token {token} was never registered");
}

async fn wait_unknown(helper: &Helper, token: &str) {
    for _ in 0..150 {
        if helper.agent_for(token).await.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("token {token} is still known");
}

async fn notify_started(
    helper: &Helper,
    peer: &TestPeer,
    agent_id: &str,
    parent_id: &str,
    depth: u64,
    token: &str,
) {
    peer.send_notification(rmcp::model::ClientNotification::CustomNotification(
        CustomNotification::new(
            "hoocode/agentStarted".to_owned(),
            Some(serde_json::json!({
                "agentId": agent_id,
                "parentId": parent_id,
                "depth": depth,
                "token": token,
            })),
        ),
    ))
    .await
    .expect("agentStarted");
    wait_registered(helper, token).await;
}

async fn notify_started_child(helper: &Helper, peer: &TestPeer, agent_id: &str, token: &str) {
    notify_started(helper, peer, agent_id, "root", 1, token).await;
}

/// Send `agentStarted` without waiting: for refused registrations the token
/// never appears, so the caller polls for continued absence instead.
async fn send_started(peer: &TestPeer, agent_id: &str, parent_id: &str, depth: u64, token: &str) {
    peer.send_notification(rmcp::model::ClientNotification::CustomNotification(
        CustomNotification::new(
            "hoocode/agentStarted".to_owned(),
            Some(serde_json::json!({
                "agentId": agent_id,
                "parentId": parent_id,
                "depth": depth,
                "token": token,
            })),
        ),
    ))
    .await
    .expect("agentStarted");
}

/// A refused registration must never surface the token (polled absence:
/// there is no event to wait for).
async fn assert_still_unknown(helper: &Helper, token: &str) {
    for _ in 0..15 {
        assert!(
            helper.agent_for(token).await.is_none(),
            "token {token} must stay unknown"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn notify_ended(helper: &Helper, peer: &TestPeer, agent_id: &str, token: &str) {
    peer.send_notification(rmcp::model::ClientNotification::CustomNotification(
        CustomNotification::new(
            "hoocode/agentEnded".to_owned(),
            Some(serde_json::json!({ "agentId": agent_id })),
        ),
    ))
    .await
    .expect("agentEnded");
    wait_unknown(helper, token).await;
}

async fn call(
    peer: &TestPeer,
    tool: &str,
    args: Value,
) -> Result<rmcp::model::CallToolResult, String> {
    let mut params = CallToolRequestParams::new(tool.to_owned());
    params.arguments = args.as_object().cloned();
    match peer.call_tool_once(params).await {
        Ok(CallToolResponse::Complete(result)) => Ok(result),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(e) => Err(format!("protocol error: {e}")),
    }
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_error(result: &rmcp::model::CallToolResult) -> bool {
    result.is_error.unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Fake backend: always runs.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fake_lists_eight_tools_and_hides_cli() {
    let dir = test_tmp("list");
    let (_helper, socket) =
        start_helper(&manifest_dir().join("tests/fake-agent-browser.sh"), &dir).await;
    let peer = connect(&socket, ROOT_TOKEN).await;
    let tools = peer
        .list_tools(Default::default())
        .await
        .expect("list_tools");
    let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names.len(), 8, "the hidden browser_cli must not be listed");
    for expected in [
        "browser_open",
        "browser_snapshot",
        "browser_act",
        "browser_screenshot",
        "browser_tabs",
        "browser_wait",
        "browser_nav",
        "browser_eval",
    ] {
        assert!(names.contains(&expected.to_owned()), "missing {expected}");
    }
}

#[tokio::test]
async fn fake_leases_limits_and_names() {
    let dir = test_tmp("leases");
    let (_helper, socket) =
        start_helper(&manifest_dir().join("tests/fake-agent-browser.sh"), &dir).await;
    let peer = connect(&socket, ROOT_TOKEN).await;
    // Tabs open in order; tab 1 is the default.
    let open = call(
        &peer,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("open tab 1");
    assert!(!is_error(&open));
    for tab in 2..=4 {
        let result = call(
            &peer,
            "browser_open",
            serde_json::json!({"url": "https://x.example/", "tab": tab}),
        )
        .await
        .expect("open tab");
        assert!(!is_error(&result), "tab {tab} must open");
    }
    // The 5th tab is over the per-agent limit.
    let over = call(
        &peer,
        "browser_open",
        serde_json::json!({"url": "https://x.example/", "tab": 5}),
    )
    .await
    .expect("protocol ok");
    assert!(
        is_error(&over),
        "tab 5 must be refused, got: {}",
        text_of(&over)
    );
    // Skipping a tab number is refused too.
    let skip = call(&peer, "browser_snapshot", serde_json::json!({"tab": 9}))
        .await
        .expect("protocol ok");
    assert!(is_error(&skip));
    // Snapshot carries the canned tree with refs.
    let snapshot = call(&peer, "browser_snapshot", serde_json::json!({}))
        .await
        .expect("snapshot");
    assert!(!is_error(&snapshot));
    assert!(
        text_of(&snapshot).contains("e1"),
        "snapshot must carry refs"
    );
    // Unknown refs come back as tool errors, not protocol errors.
    let bad = call(
        &peer,
        "browser_act",
        serde_json::json!({"action": "click", "ref": "e999"}),
    )
    .await
    .expect("protocol ok");
    assert!(is_error(&bad));
    assert!(text_of(&bad).contains("Unknown ref"));
}

#[tokio::test]
async fn fake_screenshot_returns_image_content() {
    let dir = test_tmp("shot");
    let (_helper, socket) =
        start_helper(&manifest_dir().join("tests/fake-agent-browser.sh"), &dir).await;
    let peer = connect(&socket, ROOT_TOKEN).await;
    call(
        &peer,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("open");
    let shot = call(&peer, "browser_screenshot", serde_json::json!({}))
        .await
        .expect("screenshot");
    assert!(!is_error(&shot), "screenshot failed: {}", text_of(&shot));
    let images: Vec<&ContentBlock> = shot
        .content
        .iter()
        .filter(|b| matches!(b, ContentBlock::Image(_)))
        .collect();
    assert_eq!(images.len(), 1, "screenshot must return one image block");
    let ContentBlock::Image(image) = images[0] else {
        unreachable!();
    };
    assert_eq!(image.mime_type, "image/png");
    let bytes = base64_decode(&image.data);
    assert!(
        bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "image data must be a PNG"
    );
}

fn base64_decode(data: &str) -> Vec<u8> {
    // Minimal base64 decoder (test-only): the helper encodes with STANDARD.
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut vals: Vec<u8> = Vec::new();
    for c in data.bytes() {
        if c == b'=' {
            break;
        }
        vals.push(ALPHABET.iter().position(|&a| a == c).expect("base64") as u8);
    }
    let mut out = Vec::new();
    for chunk in vals.chunks(4) {
        let mut n: u32 = 0;
        for &v in chunk {
            n = (n << 6) | v as u32;
        }
        let pad = 4 - chunk.len();
        n <<= 6 * pad;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    out
}

#[tokio::test]
async fn fake_parallel_agents_cross_agent_refused_and_lifecycle() {
    let dir = test_tmp("agents");
    let (helper, socket) =
        start_helper(&manifest_dir().join("tests/fake-agent-browser.sh"), &dir).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started_child(&helper, &root, "root/task-1", "child-1-token").await;
    notify_started_child(&helper, &root, "root/task-2", "child-2-token").await;
    let child1 = connect(&socket, "child-1-token").await;
    let child2 = connect(&socket, "child-2-token").await;

    // Two agents in parallel, two tabs each (leases: child-1 gets *-1, *-2).
    let (r1, r2) = tokio::join!(
        call(
            &child1,
            "browser_open",
            serde_json::json!({"url": "https://a.example/", "tab": 1})
        ),
        call(
            &child2,
            "browser_open",
            serde_json::json!({"url": "https://b.example/", "tab": 1})
        ),
    );
    assert!(!is_error(&r1.expect("child1 tab1")));
    assert!(!is_error(&r2.expect("child2 tab1")));
    let (r1, r2) = tokio::join!(
        call(&child1, "browser_snapshot", serde_json::json!({"tab": 2})),
        call(&child2, "browser_snapshot", serde_json::json!({"tab": 2})),
    );
    assert!(!is_error(&r1.expect("child1 tab2")));
    assert!(!is_error(&r2.expect("child2 tab2")));

    // Cross-agent access is refused: the CLI cannot name another lease.
    let cli_args = Value::Array(vec![
        Value::String("--session".into()),
        Value::String("root-1".into()),
        Value::String("snapshot".into()),
    ]);
    let mut params = Map::new();
    params.insert("args".into(), cli_args);
    let refused = call(&child1, "browser_cli", Value::Object(params)).await;
    assert!(
        refused.is_err(),
        "browser_cli on another agent's session must be a protocol refusal, got {refused:?}"
    );

    // A non-child registration is ignored: the token never becomes known.
    send_started(&child1, "root/task-9", "root", 1, "rogue-token").await;
    assert_still_unknown(&helper, "rogue-token").await;
    let rogue = try_connect(&socket, "rogue-token").await;
    assert!(rogue.is_err(), "rogue token must be unknown: {rogue:?}");

    // agentEnded closes that agent's sessions and drops its token.
    notify_ended(&helper, &root, "root/task-2", "child-2-token").await;
    let ended_call = call(&child2, "browser_snapshot", serde_json::json!({})).await;
    assert!(
        ended_call.is_err(),
        "ended agent's token must be unknown: {ended_call:?}"
    );
    // The other agent is unaffected.
    let still_there = call(&child1, "browser_snapshot", serde_json::json!({}))
        .await
        .expect("child1 survives");
    assert!(!is_error(&still_there));

    // Root adopts child1's tabs, then child1 ending loses nothing.
    let adopt = call(
        &root,
        "browser_tabs",
        serde_json::json!({"action": "adopt", "from": "root/task-1"}),
    )
    .await
    .expect("adopt");
    assert!(!is_error(&adopt), "adopt failed: {}", text_of(&adopt));
    notify_ended(&helper, &root, "root/task-1", "child-1-token").await;
    let list = call(&root, "browser_tabs", serde_json::json!({"action": "list"}))
        .await
        .expect("list");
    assert!(!is_error(&list));
    assert!(
        text_of(&list).contains("root_task-1-1"),
        "adopted lease must survive: {}",
        text_of(&list)
    );
}

/// Wrapper around the fake so each test gets its own argv log and fail
/// file (process env is shared by parallel tests, files are not).
fn fake_wrapper(dir: &Path, argv_log: &Path, fail_file: Option<&Path>) -> PathBuf {
    fn sh_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }
    let path = dir.join("fake-wrapper.sh");
    let mut script = format!("#!/bin/bash\nexport FAKE_ARGV_LOG={}\n", sh_quote(argv_log));
    if let Some(fail) = fail_file {
        script.push_str(&format!("export FAKE_FAIL_FILE={}\n", sh_quote(fail)));
    }
    script.push_str(&format!(
        "exec {} \"$@\"\n",
        sh_quote(&manifest_dir().join("tests/fake-agent-browser.sh"))
    ));
    std::fs::write(&path, script).expect("wrapper");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path
}

fn argv_lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn fake_every_call_carries_session_and_pin_tab() {
    let dir = test_tmp("argv");
    let argv_log = dir.join("argv.log");
    let fake = fake_wrapper(&dir, &argv_log, None);
    let (helper, socket) = start_helper(&fake, &dir).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started_child(&helper, &root, "root/task-1", "child-1-token").await;
    let child = connect(&socket, "child-1-token").await;
    for tab in [1, 2] {
        let opened = call(
            &child,
            "browser_open",
            serde_json::json!({"url": "https://x.example/", "tab": tab}),
        )
        .await
        .expect("open");
        assert!(
            !is_error(&opened),
            "open tab {tab} failed: {}",
            text_of(&opened)
        );
    }
    call(&child, "browser_snapshot", serde_json::json!({}))
        .await
        .expect("snapshot tab 1");
    let cli = call(
        &child,
        "browser_cli",
        serde_json::json!({"args": ["snapshot"]}),
    )
    .await
    .expect("cli");
    assert!(!is_error(&cli));
    let lines = argv_lines(&argv_log);
    // open tab 1 (window new + open), open tab 2 (window new + open),
    // snapshot tab 1, cli snapshot on the default lease: every spawn scoped.
    assert!(
        lines.len() >= 6,
        "expected several recorded calls, got {lines:?}"
    );
    for line in &lines {
        assert!(line.contains("--pin-tab"), "leases pin their tab: {line}");
        assert!(
            line.contains("--cdp ws://fake-endpoint-for-tests"),
            "shared browser is not optional: {line}"
        );
    }
    let tab1: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("root_task-1-1"))
        .collect();
    let tab2: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("root_task-1-2"))
        .collect();
    assert!(
        !tab1.is_empty() && !tab2.is_empty(),
        "both tabs used: {lines:?}"
    );
    assert!(
        lines.iter().all(|l| l.contains("root_task-1-")),
        "no call escapes the agent's leases: {lines:?}"
    );
}

#[tokio::test]
async fn fake_window_new_failure_rolls_the_lease_back() {
    let dir = test_tmp("rollback");
    let argv_log = dir.join("argv.log");
    let fail_file = dir.join("fail");
    let fake = fake_wrapper(&dir, &argv_log, Some(&fail_file));
    let (helper, socket) = start_helper(&fake, &dir).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started_child(&helper, &root, "root/task-1", "child-1-token").await;
    let child = connect(&socket, "child-1-token").await;
    call(
        &child,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("open tab 1");
    // Fail tab 2's window: the open errors and the lease is rolled back.
    std::fs::write(&fail_file, "root_task-1-2\n").expect("fail file");
    let bad = call(
        &child,
        "browser_open",
        serde_json::json!({"url": "https://x.example/", "tab": 2}),
    )
    .await
    .expect("protocol ok");
    assert!(
        is_error(&bad),
        "window-new failure must surface: {}",
        text_of(&bad)
    );
    // No phantom tab: the failed window never ran an open, and tab 1 is
    // untouched.
    let calls = argv_lines(&argv_log);
    let opened_tab2 = |calls: &[String]| {
        calls
            .iter()
            .any(|l| l.contains("--session root_task-1-2") && l.contains(" open "))
    };
    assert!(
        !opened_tab2(&calls),
        "no open ran on the failed lease: {calls:?}"
    );
    let tab1 = call(&child, "browser_snapshot", serde_json::json!({}))
        .await
        .expect("tab 1 survives");
    assert!(!is_error(&tab1));
    // The failure lifted: tab 2 opens under its canonical name, proving
    // the rollback freed the lease (a stale lease would force a suffix).
    std::fs::remove_file(&fail_file).expect("unfail");
    let tab2 = call(
        &child,
        "browser_open",
        serde_json::json!({"url": "https://x.example/", "tab": 2}),
    )
    .await
    .expect("tab 2 reopens");
    assert!(!is_error(&tab2), "tab 2 must reopen: {}", text_of(&tab2));
    let calls = argv_lines(&argv_log);
    assert!(
        opened_tab2(&calls),
        "tab 2 reuses its name after rollback: {calls:?}"
    );
}

#[tokio::test]
async fn fake_closing_a_tab_keeps_the_others() {
    let dir = test_tmp("closekeep");
    let (helper, socket) =
        start_helper(&manifest_dir().join("tests/fake-agent-browser.sh"), &dir).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started_child(&helper, &root, "root/task-1", "child-1-token").await;
    let child = connect(&socket, "child-1-token").await;
    for tab in [1, 2] {
        call(
            &child,
            "browser_open",
            serde_json::json!({"url": "https://x.example/", "tab": tab}),
        )
        .await
        .expect("open");
    }
    let closed = call(
        &child,
        "browser_tabs",
        serde_json::json!({"action": "close", "tab": 1}),
    )
    .await
    .expect("close tab 1");
    assert!(!is_error(&closed), "close failed: {}", text_of(&closed));
    // Tab 2 is unaffected (no renumbering); tab 1 reopens fresh.
    let tab2 = call(&child, "browser_snapshot", serde_json::json!({"tab": 2}))
        .await
        .expect("tab 2");
    assert!(!is_error(&tab2), "tab 2 must survive: {}", text_of(&tab2));
    let reopened = call(
        &child,
        "browser_open",
        serde_json::json!({"url": "https://y.example/", "tab": 1}),
    )
    .await
    .expect("reopen tab 1");
    assert!(
        !is_error(&reopened),
        "tab 1 must reopen: {}",
        text_of(&reopened)
    );
}

#[tokio::test]
async fn fake_spawn_and_timeout_failures_are_tool_errors() {
    // No such binary: the spawn fails and the model sees a tool error.
    let dir = test_tmp("spawnfail");
    let socket = dir.join("helper.sock");
    let helper = Helper::new(
        PathBuf::from("/nonexistent/agent-browser-for-tests"),
        "ws://fake".into(),
        dir.clone(),
        hoo_browser::backend::socket_dir(&dir),
        ROOT_TOKEN.into(),
        "root".into(),
    );
    tokio::spawn(hoo_browser::server::serve(helper, socket.clone()));
    wait_for_socket(&socket).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    let failed = call(
        &root,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("protocol ok");
    assert!(is_error(&failed), "spawn failure must be a tool error");
    assert!(
        text_of(&failed).contains("cannot spawn"),
        "got: {}",
        text_of(&failed)
    );

    // A hanging agent-browser trips the per-call timeout (lowered for the
    // test; concurrent fake calls take milliseconds, far under either).
    let dir = test_tmp("timeout");
    let argv_log = dir.join("argv.log");
    let fake = fake_wrapper(&dir, &argv_log, None);
    let (helper, socket) = start_helper(&fake, &dir).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started_child(&helper, &root, "root/slowpoke", "slow-token").await;
    let slow = connect(&socket, "slow-token").await;
    std::env::set_var("HOO_BROWSER_CALL_TIMEOUT_SECS", "1");
    let timed = call(
        &slow,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("protocol ok");
    std::env::remove_var("HOO_BROWSER_CALL_TIMEOUT_SECS");
    assert!(is_error(&timed), "timeout must be a tool error");
    assert!(
        text_of(&timed).contains("timed out"),
        "got: {}",
        text_of(&timed)
    );
}

/// Send `agentEnded` without waiting (ignored ones change nothing).
async fn send_ended(peer: &TestPeer, agent_id: &str) {
    peer.send_notification(rmcp::model::ClientNotification::CustomNotification(
        CustomNotification::new(
            "hoocode/agentEnded".to_owned(),
            Some(serde_json::json!({ "agentId": agent_id })),
        ),
    ))
    .await
    .expect("agentEnded");
    // Notifications are async: give the server a chance to (not) act.
    tokio::time::sleep(Duration::from_millis(150)).await;
}

async fn send_session_ending(peer: &TestPeer) {
    peer.send_notification(rmcp::model::ClientNotification::CustomNotification(
        CustomNotification::new("hoocode/sessionEnding".to_owned(), None),
    ))
    .await
    .expect("sessionEnding");
    tokio::time::sleep(Duration::from_millis(150)).await;
}

#[tokio::test]
async fn fake_registration_scoping_and_ending_rules() {
    let dir = test_tmp("rules");
    let (helper, socket) =
        start_helper(&manifest_dir().join("tests/fake-agent-browser.sh"), &dir).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started_child(&helper, &root, "root/task-1", "child-1-token").await;
    // C1 over the wire: none of these nest under the sender correctly.
    send_started(&root, "other/task-9", "root", 1, "rogue-a").await; // parent mismatch
    send_started(&root, "root2/x", "root", 1, "rogue-b").await; // not under root/
    send_started(&root, "root/task-1", "root", 1, "rogue-c").await; // taken id
    send_started(&root, "root/task-9", "root", 2, "rogue-d").await; // depth skip
    send_started(&root, "root/task-9", "root", 1, "child-1-token").await; // taken token
    for token in ["rogue-a", "rogue-b", "rogue-c", "rogue-d"] {
        assert_still_unknown(&helper, token).await;
    }
    assert_still_unknown(&helper, "child-1-token-reuse-never-exists").await;
    // The reused token still maps to its original agent.
    assert!(helper.agent_for("child-1-token").await.is_some());
    // H1: a sibling cannot end another agent, nor itself, and a non-root
    // sessionEnding is noise.
    let child = connect(&socket, "child-1-token").await;
    send_ended(&child, "root").await;
    send_ended(&child, "root/task-1").await;
    send_session_ending(&child).await;
    let root_list = call(&root, "browser_tabs", serde_json::json!({"action": "list"}))
        .await
        .expect("root survives sibling ends");
    assert!(!is_error(&root_list));
    let child_snap = call(&child, "browser_snapshot", serde_json::json!({}))
        .await
        .expect("child survives self end");
    assert!(!is_error(&child_snap));
}

#[tokio::test]
async fn fake_unknown_token_gets_401() {
    let dir = test_tmp("auth");
    let (_helper, socket) =
        start_helper(&manifest_dir().join("tests/fake-agent-browser.sh"), &dir).await;
    // The handshake itself must fail: every request needs a known token.
    let result = try_connect(&socket, "wrong-token").await;
    assert!(result.is_err(), "unknown token must get 401: {result:?}");
}

#[test]
fn overhead_report_exists_after_fake_calls() {
    // Runs after the async fake tests above (same temp roots differ, so find
    // any log): parse overhead_ms lines and report p50/p95.
    let mut overheads: Vec<u128> = Vec::new();
    let tmp = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&tmp) else {
        return;
    };
    for entry in entries.flatten() {
        let log = entry.path().join("hoo-browser.log");
        let Ok(text) = std::fs::read_to_string(&log) else {
            continue;
        };
        for line in text.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                if let Some(us) = value.get("overhead_us").and_then(Value::as_u64) {
                    overheads.push(us as u128);
                }
            }
        }
    }
    if overheads.is_empty() {
        println!("no overhead lines found (fake tests may not have run yet)");
        return;
    }
    overheads.sort_unstable();
    let pick = |p: f64| overheads[((overheads.len() as f64 * p) as usize).min(overheads.len() - 1)];
    println!(
        "overhead_us over {} calls: p50={} p95={} max={} (see scripts/overhead.py)",
        overheads.len(),
        pick(0.5),
        pick(0.95),
        overheads.last().unwrap()
    );
}

// ---------------------------------------------------------------------------
// Real backend: real agent-browser + headless Chromium, behind HOO_BROWSER_IT.
// ---------------------------------------------------------------------------

fn real_backend() -> Option<PathBuf> {
    if std::env::var("HOO_BROWSER_IT").as_deref() != Ok("1") {
        return None;
    }
    let path = std::env::var("HOO_BROWSER_AGENT_BROWSER")
        .map(PathBuf::from)
        .expect("HOO_BROWSER_IT=1 needs HOO_BROWSER_AGENT_BROWSER pointing at a real agent-browser binary");
    assert!(
        path.is_file(),
        "agent-browser binary missing: {}",
        path.display()
    );
    Some(path)
}

/// Tiny HTTP page server for the no-steal suite: `/` links to `/two` and has
/// a button that rewrites its own label when clicked.
async fn start_test_page() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                // Read until the headers complete: a single read can split
                // the request under a navigation burst.
                let mut seen = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    if seen.len() > 65536 || seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&seen);
                let path = request
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/");
                let body = if path == "/two" {
                    "<html><head><title>two</title></head><body><h1>page two</h1><a href=\"/\">back</a></body></html>"
                } else {
                    "<html><head><title>one</title></head><body><h1>page one</h1><a href=\"/two\">go to two</a><button onclick=\"this.textContent='clicked'\">press me</button></body></html>"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/"), handle)
}

fn ref_for(result_text: &str, label: &str) -> Option<String> {
    // The tool result is pretty-printed JSON: parse it and scan the real
    // snapshot lines, which look like: - link "go to two" [ref=eN].
    // (Scanning the raw JSON misfires: one JSON line holds every tree line.)
    let value: Value = serde_json::from_str(result_text).ok()?;
    let snapshot = value.get("snapshot")?.as_str()?;
    for line in snapshot.lines() {
        if line.contains(label) {
            if let Some(at) = line.find("[ref=") {
                let rest = &line[at + 5..];
                if let Some(end) = rest.find(']') {
                    return Some(rest[..end].to_owned());
                }
            }
        }
    }
    None
}

/// Page targets on the shared browser via the DevTools HTTP endpoint.
async fn cdp_page_targets(endpoint: &str) -> usize {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let bare = endpoint
        .trim_start_matches("ws://")
        .trim_start_matches("wss://");
    let host_port = bare.split('/').next().unwrap_or("").to_owned();
    let mut stream = tokio::net::TcpStream::connect(&host_port)
        .await
        .expect("cdp http");
    let request =
        format!("GET /json/list HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut body = Vec::new();
    stream.read_to_end(&mut body).await.expect("read");
    let text = String::from_utf8_lossy(&body);
    let json_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    serde_json::from_str::<Value>(&text[json_start..])
        .ok()
        .and_then(|targets| targets.as_array().cloned())
        .map(|targets| {
            targets
                .iter()
                .filter(|t| t.get("type") == Some(&Value::String("page".into())))
                .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn real_no_steal_suite() {
    let Some(agent_browser) = real_backend() else {
        println!("skipping real backend (HOO_BROWSER_IT=1 not set)");
        return;
    };
    let dir = test_tmp("real");
    let chrome = hoo_browser::backend::locate_chromium().expect("chromium for IT");
    let chromium = hoo_browser::backend::launch_chromium(&chrome, &dir, &dir, false)
        .await
        .expect("launch chromium");
    let helper = Helper::new(
        agent_browser,
        chromium.endpoint.clone(),
        dir.clone(),
        hoo_browser::backend::socket_dir(&dir),
        ROOT_TOKEN.into(),
        "root".into(),
    );
    let socket = dir.join("helper.sock");
    let _ = std::fs::remove_file(&socket);
    tokio::spawn(hoo_browser::server::serve(helper.clone(), socket.clone()));
    wait_for_socket(&socket).await;
    let endpoint = chromium.endpoint.clone();
    // Chromium dies with the test process (kill_on_drop in the handle).
    let _chromium = chromium;

    let (page, _page_server) = start_test_page().await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started_child(&helper, &root, "root/task-1", "child-1-token").await;
    notify_started_child(&helper, &root, "root/task-2", "child-2-token").await;
    notify_started_child(&helper, &root, "root/task-3", "child-3-token").await;
    let children = [
        connect(&socket, "child-1-token").await,
        connect(&socket, "child-2-token").await,
        connect(&socket, "child-3-token").await,
    ];

    // Open the local page, snapshot, click the link by ref, land on /two.
    let open = call(&root, "browser_open", serde_json::json!({"url": page}))
        .await
        .expect("open");
    assert!(!is_error(&open), "open failed: {}", text_of(&open));
    let snapshot = call(&root, "browser_snapshot", serde_json::json!({}))
        .await
        .expect("snapshot");
    assert!(
        !is_error(&snapshot),
        "snapshot failed: {}",
        text_of(&snapshot)
    );
    let tree = text_of(&snapshot);
    let link = ref_for(&tree, "go to two").expect("link ref in snapshot");
    let click = call(
        &root,
        "browser_act",
        serde_json::json!({"action": "click", "ref": link}),
    )
    .await
    .expect("click");
    assert!(!is_error(&click), "click failed: {}", text_of(&click));
    let after = call(&root, "browser_snapshot", serde_json::json!({}))
        .await
        .expect("snapshot 2");
    assert!(
        text_of(&after).contains("page two"),
        "must land on /two: {}",
        text_of(&after)
    );

    // Screenshot returns a real PNG.
    let shot = call(&root, "browser_screenshot", serde_json::json!({}))
        .await
        .expect("screenshot");
    assert!(!is_error(&shot), "screenshot failed: {}", text_of(&shot));
    let images: Vec<&ContentBlock> = shot
        .content
        .iter()
        .filter(|b| matches!(b, ContentBlock::Image(_)))
        .collect();
    assert_eq!(images.len(), 1);
    let ContentBlock::Image(image) = images[0] else {
        unreachable!();
    };
    assert_eq!(image.mime_type, "image/png");
    assert!(base64_decode(&image.data).starts_with(&[0x89, b'P', b'N', b'G']));

    // Back to /one, press the button via its ref, confirm the label flips.
    call(&root, "browser_nav", serde_json::json!({"action": "back"}))
        .await
        .expect("back");
    let back = call(&root, "browser_snapshot", serde_json::json!({}))
        .await
        .expect("snapshot 3");
    let button = ref_for(&text_of(&back), "press me").expect("button ref");
    let press = call(
        &root,
        "browser_act",
        serde_json::json!({"action": "click", "ref": button}),
    )
    .await
    .expect("button click");
    assert!(!is_error(&press));
    // The click runs JS; the tree should show the new label.
    let mut flipped = false;
    for _ in 0..10 {
        let again = call(&root, "browser_snapshot", serde_json::json!({}))
            .await
            .expect("re-snapshot");
        if text_of(&again).contains("clicked") {
            flipped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(flipped, "button label must flip to 'clicked'");

    // 3 agents in parallel, 2 tabs each, all against the local page. Each
    // task owns its connection, so the futures are Send.
    let mut tasks = Vec::new();
    let tokens = ["child-1-token", "child-2-token", "child-3-token"];
    for (i, token) in tokens.iter().enumerate() {
        for tab in 1..=2u64 {
            let url = format!("{page}?agent={i}&tab={tab}");
            let socket = socket.clone();
            let token = token.to_string();
            tasks.push(tokio::spawn(async move {
                let peer = connect(&socket, &token).await;
                let mut opened = call(
                    &peer,
                    "browser_open",
                    serde_json::json!({"url": url, "tab": tab}),
                )
                .await;
                // A navigation burst can abort one load; the helper reports
                // it as a tool error and a single retry lands it.
                if let Ok(result) = &opened {
                    if is_error(result) && text_of(result).contains("ERR_ABORTED") {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        opened = call(
                            &peer,
                            "browser_open",
                            serde_json::json!({"url": url, "tab": tab}),
                        )
                        .await;
                    }
                }
                let snap = call(&peer, "browser_snapshot", serde_json::json!({"tab": tab})).await;
                // location.href proves the tab really is this agent's page:
                // a shared or stolen session would show another query.
                let href = call(
                    &peer,
                    "browser_eval",
                    serde_json::json!({"js": "location.href", "tab": tab}),
                )
                .await;
                (i, tab, opened, snap, href)
            }));
        }
    }
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.expect("parallel task"));
    }
    for (i, tab, opened, snap, href) in results {
        let opened = opened.expect("parallel open");
        assert!(
            !is_error(&opened),
            "agent {i} tab {tab} open failed: {}",
            text_of(&opened)
        );
        let snap = snap.expect("parallel snapshot");
        assert!(
            !is_error(&snap),
            "agent {i} tab {tab} snapshot failed: {}",
            text_of(&snap)
        );
        assert!(
            text_of(&snap).contains("page one"),
            "agent {i} tab {tab} sees its own page"
        );
        let href = href.expect("parallel href");
        assert!(
            !is_error(&href),
            "agent {i} tab {tab} href failed: {}",
            text_of(&href)
        );
        let want = format!("agent={i}&tab={tab}");
        assert!(
            text_of(&href).contains(&want),
            "agent {i} tab {tab} must be on its own page (?{want}), got: {}",
            text_of(&href)
        );
    }

    // Cross-agent access is refused over the CLI fallback too.
    let cross = call(
        &children[0],
        "browser_cli",
        serde_json::json!({"args": ["--session", "root-1", "snapshot"]}),
    )
    .await;
    assert!(cross.is_err(), "cross-agent CLI must be refused: {cross:?}");

    // agentEnded closes that agent's sessions and windows: the DevTools
    // /json endpoint must lose at least its two page targets.
    let pages_before = cdp_page_targets(&endpoint).await;
    assert!(pages_before >= 7, "leases must own pages: {pages_before}");
    notify_ended(&helper, &root, "root/task-3", "child-3-token").await;
    let ended = call(&children[2], "browser_snapshot", serde_json::json!({})).await;
    assert!(ended.is_err(), "ended agent must be unknown: {ended:?}");
    let mut pages_after = pages_before;
    for _ in 0..50 {
        pages_after = cdp_page_targets(&endpoint).await;
        if pages_after + 2 <= pages_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        pages_after + 2 <= pages_before,
        "agentEnded must close windows too: {pages_after} vs {pages_before} page targets"
    );

    report_overheads(&dir);
    // Chromium is killed by `_chromium` drop at test end.
}

fn report_overheads(dir: &Path) {
    let text = std::fs::read_to_string(dir.join("hoo-browser.log")).unwrap_or_default();
    let mut overheads: Vec<u128> = Vec::new();
    let mut children: Vec<u128> = Vec::new();
    let mut totals: Vec<u128> = Vec::new();
    for line in text.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if let Some(us) = value.get("overhead_us").and_then(Value::as_u64) {
                overheads.push(us as u128);
            }
            if let Some(us) = value.get("child_us").and_then(Value::as_u64) {
                children.push(us as u128);
            }
            if let Some(us) = value.get("total_us").and_then(Value::as_u64) {
                totals.push(us as u128);
            }
        }
    }
    if overheads.is_empty() {
        println!("no overhead lines logged");
        return;
    }
    overheads.sort_unstable();
    children.sort_unstable();
    totals.sort_unstable();
    let pick = |v: &[u128], p: f64| v[((v.len() as f64 * p) as usize).min(v.len() - 1)];
    println!(
        "calls={} helper-only overhead_us p50={} p95={} max={} | spawn+connect child_us p50={} p95={} | total_us p50={} p95={} (see scripts/overhead.py)",
        overheads.len(),
        pick(&overheads, 0.5),
        pick(&overheads, 0.95),
        overheads.last().unwrap(),
        pick(&children, 0.5),
        pick(&children, 0.95),
        pick(&totals, 0.5),
        pick(&totals, 0.95),
    );
}
