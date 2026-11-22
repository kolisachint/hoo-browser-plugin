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
    start_helper_cdp(agent_browser, data_dir, "ws://fake-endpoint-for-tests").await
}

/// `start_helper` with a custom Chromium endpoint: mode tests point it at
/// a fake DevTools listener so they can observe which targets a close
/// removes.
async fn start_helper_cdp(agent_browser: &Path, data_dir: &Path, cdp: &str) -> (Helper, PathBuf) {
    // B6: the helper-owned config is created once at startup (0600),
    // like main.rs does; spawns only read the cached path.
    let sock_dir = hoo_browser::backend::socket_dir(data_dir);
    hoo_browser::exec::ensure_agent_browser_config(&sock_dir).expect("helper config");
    let socket = data_dir.join("helper.sock");
    let helper = Helper::new(
        agent_browser.to_owned(),
        cdp.into(),
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
    // Any free slot opens directly; tab 1 is the default.
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
    // A never-opened slot errors with the open-it hint.
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
async fn fake_open_any_free_slot_and_hint_on_free_slots() {
    // Any free slot opens directly: `browser_open` with tab 3 first works
    // (no need to fill lower slots), and other tools on a free slot
    // error with a hint to open it via `browser_open`.
    let dir = test_tmp("anyslot");
    let (_helper, socket) =
        start_helper(&manifest_dir().join("tests/fake-agent-browser.sh"), &dir).await;
    let peer = connect(&socket, ROOT_TOKEN).await;
    let third = call(
        &peer,
        "browser_open",
        serde_json::json!({"url": "https://x.example/", "tab": 3}),
    )
    .await
    .expect("open tab 3");
    assert!(
        !is_error(&third),
        "tab 3 must open first: {}",
        text_of(&third)
    );
    for tab in [1, 2, 4] {
        let snap = call(&peer, "browser_snapshot", serde_json::json!({"tab": tab}))
            .await
            .expect("protocol ok");
        assert!(is_error(&snap), "tab {tab} is still free");
        let text = text_of(&snap);
        assert!(
            text.contains(&format!("tab {tab} is not open")) && text.contains("browser_open"),
            "free slots hint at browser_open, got: {text}"
        );
    }
    let snap = call(&peer, "browser_snapshot", serde_json::json!({"tab": 3}))
        .await
        .expect("snapshot tab 3");
    assert!(!is_error(&snap), "tab 3 works: {}", text_of(&snap));
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
    // Only `browser_open` creates tabs: each agent opens its slots, then
    // snapshots them.
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
        call(
            &child1,
            "browser_open",
            serde_json::json!({"url": "https://a.example/", "tab": 2})
        ),
        call(
            &child2,
            "browser_open",
            serde_json::json!({"url": "https://b.example/", "tab": 2})
        ),
    );
    assert!(!is_error(&r1.expect("child1 open tab2")));
    assert!(!is_error(&r2.expect("child2 open tab2")));
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
fn fake_wrapper(
    dir: &Path,
    argv_log: &Path,
    fail_file: Option<&Path>,
    fail_once: Option<&Path>,
) -> PathBuf {
    fn sh_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }
    let path = dir.join("fake-wrapper.sh");
    let mut script = format!(
        "#!/bin/bash\nexport FAKE_ARGV_LOG={}\nexport FAKE_ENV_LOG={}.env\n",
        sh_quote(argv_log),
        sh_quote(argv_log)
    );
    if let Some(fail) = fail_file {
        script.push_str(&format!("export FAKE_FAIL_FILE={}\n", sh_quote(fail)));
    }
    if let Some(once_dir) = fail_once {
        script.push_str(&format!(
            "export FAKE_FAIL_ONCE_DIR={}\n",
            sh_quote(once_dir)
        ));
    }
    // The wrapper path is the binary path the helper knows; the inner
    // script names the fake daemon after it (argv[0]), so the helper's
    // shutdown command-line check accepts the fake daemon exactly like
    // the real one. A plain `exec -a` here would not survive: the kernel
    // drops argv[0] when re-execing a shebang script's interpreter.
    script.push_str("export FAKE_AB_SELF=\"$0\"\n");
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
    let fake = fake_wrapper(&dir, &argv_log, None, None);
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
async fn fake_agent_browser_config_is_helper_owned() {
    // N6: every agent-browser child runs with `AGENT_BROWSER_CONFIG`
    // pointing at a helper-owned file containing `{}`, so the user's
    // `~/.agent-browser/config.json` cannot change helper output.
    let dir = test_tmp("config");
    let argv_log = dir.join("argv.log");
    let env_log = dir.join("argv.log.env");
    let fake = fake_wrapper(&dir, &argv_log, None, None);
    let (_helper, socket) = start_helper(&fake, &dir).await;
    let peer = connect(&socket, ROOT_TOKEN).await;
    call(
        &peer,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("open");
    call(&peer, "browser_snapshot", serde_json::json!({}))
        .await
        .expect("snapshot");
    let argv_calls = argv_lines(&argv_log);
    let env_lines: Vec<String> = std::fs::read_to_string(&env_log)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(
        env_lines.len(),
        argv_calls.len(),
        "every child must log its config env: {env_lines:?} vs {argv_calls:?}"
    );
    assert!(!env_lines.is_empty());
    for line in &env_lines {
        assert!(
            line.ends_with("agent-browser-config.json"),
            "config must be the helper-owned file, got: {line}"
        );
        let content = std::fs::read_to_string(line.trim())
            .unwrap_or_else(|e| panic!("cannot read {line}: {e}"));
        assert_eq!(content.trim(), "{}", "helper config must be empty JSON");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(env_lines[0].trim())
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "helper config must be owner-only");
    }
}

#[tokio::test]
async fn fake_window_new_failure_rolls_the_lease_back() {
    let dir = test_tmp("rollback");
    let argv_log = dir.join("argv.log");
    let fail_file = dir.join("fail");
    let fake = fake_wrapper(&dir, &argv_log, Some(&fail_file), None);
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
    // A post-send failure (not a connect-phase one) must not be retried:
    // re-sending could double-execute. Exactly one `window new` ran.
    assert_eq!(
        calls
            .iter()
            .filter(|l| l.contains("--session root_task-1-2") && l.contains("window new"))
            .count(),
        1,
        "a failed window new must run exactly once: {calls:?}"
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
async fn fake_connect_phase_failure_retries_window_new_once() {
    // Cold-start race (N7): the first `window new` fails at connect time
    // (the daemon is not accepting yet), so the command provably never
    // ran. The helper retries once and the open succeeds with exactly
    // one `open`; the retry's `window new` is the only second one.
    let dir = test_tmp("connect-retry");
    let argv_log = dir.join("argv.log");
    let once_dir = dir.join("fail-once");
    std::fs::create_dir_all(&once_dir).expect("once dir");
    // A dedicated agent: no other test uses this session, so the
    // process-global hook cannot leak into (or be consumed by) them.
    std::fs::write(
        once_dir.join("root_connect-retry-1"),
        "Failed to connect: Connection refused (os error 61)",
    )
    .expect("arm one-shot");
    let fake = fake_wrapper(&dir, &argv_log, None, Some(&once_dir));
    let (helper, socket) = start_helper(&fake, &dir).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started_child(&helper, &root, "root/connect-retry", "retry-token").await;
    let retry = connect(&socket, "retry-token").await;
    let opened = call(
        &retry,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("protocol ok");
    assert!(
        !is_error(&opened),
        "open must succeed after the connect-phase retry: {}",
        text_of(&opened)
    );
    let calls = argv_lines(&argv_log);
    assert_eq!(
        calls
            .iter()
            .filter(|l| l.contains("--session root_connect-retry-1") && l.contains("window new"))
            .count(),
        2,
        "the failed window new runs exactly twice (one retry): {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|l| l.contains("--session root_connect-retry-1") && l.contains(" open "))
            .count(),
        1,
        "the retried lease opens exactly once: {calls:?}"
    );
}

#[tokio::test]
async fn fake_post_send_failure_never_retries() {
    // The reported cold-start symptom replayed: the first `window new`
    // fails after the send (read EAGAIN, outcome unknown), so the command
    // may have run. The helper must surface the error, not retry: exactly
    // one `window new`, no `open`, and the lease rolls back.
    let dir = test_tmp("send-noretry");
    let argv_log = dir.join("argv.log");
    let once_dir = dir.join("fail-once");
    std::fs::create_dir_all(&once_dir).expect("once dir");
    std::fs::write(
        once_dir.join("root_send-noretry-1"),
        "Failed to read: Resource temporarily unavailable (os error 35) (outcome_unknown: the command may have run, so it was not retried)",
    )
    .expect("arm one-shot");
    let fake = fake_wrapper(&dir, &argv_log, None, Some(&once_dir));
    let (helper, socket) = start_helper(&fake, &dir).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started_child(&helper, &root, "root/send-noretry", "noretry-token").await;
    let agent = connect(&socket, "noretry-token").await;
    let opened = call(
        &agent,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("protocol ok");
    assert!(is_error(&opened), "post-send failure must surface");
    assert!(
        text_of(&opened).contains("os error 35"),
        "the daemon error reaches the model: {}",
        text_of(&opened)
    );
    let calls = argv_lines(&argv_log);
    assert_eq!(
        calls
            .iter()
            .filter(|l| l.contains("--session root_send-noretry-1") && l.contains("window new"))
            .count(),
        1,
        "a post-send failure runs exactly once (never retried): {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|l| l.contains("--session root_send-noretry-1") && l.contains(" open ")),
        "no open runs on the failed lease: {calls:?}"
    );
}

#[tokio::test]
async fn fake_page_error_mentioning_connect_is_never_retried() {
    // B1: the daemon reports a page/JS failure whose text starts with the
    // client connect prefix (exit 0 envelope, like the real daemon
    // wrapping a page error). It proves nothing about delivery — the
    // daemon answered — so the helper surfaces it without retrying:
    // exactly one eval spawn, and the lease stays usable.
    let dir = test_tmp("pageerr-noretry");
    let argv_log = dir.join("argv.log");
    let fake = fake_wrapper(&dir, &argv_log, None, None);
    let (_helper, socket) = start_helper(&fake, &dir).await;
    let peer = connect(&socket, ROOT_TOKEN).await;
    call(
        &peer,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("open");
    let failed = call(
        &peer,
        "browser_eval",
        serde_json::json!({"js": "FAILCONNECT-PAGE"}),
    )
    .await
    .expect("protocol ok");
    assert!(is_error(&failed), "page failure must be a tool error");
    assert!(
        text_of(&failed).contains("simulated page error"),
        "the daemon error reaches the model: {}",
        text_of(&failed)
    );
    let calls = argv_lines(&argv_log);
    assert_eq!(
        calls.iter().filter(|l| l.contains(" eval ")).count(),
        1,
        "a page failure runs exactly once (never retried): {calls:?}"
    );
    let snapshot = call(&peer, "browser_snapshot", serde_json::json!({}))
        .await
        .expect("snapshot");
    assert!(
        !is_error(&snapshot),
        "lease survives: {}",
        text_of(&snapshot)
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

/// A fake DevTools endpoint: records each `GET /json/close/<targetId>`
/// so mode tests can observe which targets a lease close removes.
async fn fake_devtools() -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let closed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = closed.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
                let mut stream = stream;
                let mut lines = tokio::io::BufReader::new(&mut stream).lines();
                let first = lines.next_line().await.ok().flatten().unwrap_or_default();
                if let Some(path) = first.split_whitespace().nth(1) {
                    seen.lock().unwrap().push(path.to_owned());
                }
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        break;
                    }
                }
                let _ = lines
                    .into_inner()
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await;
            });
        }
    });
    (
        format!("ws://127.0.0.1:{port}/devtools/browser/fake"),
        closed,
        task,
    )
}

#[tokio::test]
async fn fake_version_probe_detects_fork_and_upstream() {
    // B3: the mode comes from `agent-browser --version`: `-hoo.` in the
    // output means the fork (scoped), anything else means safe.
    // The override travels per call, never via process env: parallel
    // tests share the process, so `set_var` here would leak into other
    // tests' helpers (notably the real suite's `probe_ab_scoped`).
    use hoo_browser::backend;
    fn version_fake(dir: &Path, name: &str, version: &str) -> PathBuf {
        let path = dir.join(name);
        let script = format!(
            "#!/bin/bash\nexport FAKE_VERSION={version}\nexport FAKE_AB_SELF=\"$0\"\nexec '{}' \"$@\"\n",
            manifest_dir()
                .join("tests/fake-agent-browser.sh")
                .to_string_lossy()
                .replace('\'', "'\\''")
        );
        std::fs::write(&path, script).expect("write version fake");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    }
    let dir = test_tmp("version-probe");
    let fork_fake = version_fake(&dir, "fork-fake.sh", "0.43.1-hoo.1");
    let upstream_fake = version_fake(&dir, "upstream-fake.sh", "0.43.0");
    let fork = backend::probe_ab_scoped_with(&fork_fake, None).await;
    let upstream = backend::probe_ab_scoped_with(&upstream_fake, None).await;
    // A forced value short-circuits without spawning: even a missing
    // binary answers.
    let missing = dir.join("no-such-binary");
    let forced_on = backend::probe_ab_scoped_with(&missing, Some(true)).await;
    let forced_off = backend::probe_ab_scoped_with(&missing, Some(false)).await;
    assert!(fork, "a -hoo.N version must mean scoped mode");
    assert!(!upstream, "an upstream version must mean safe mode");
    assert!(forced_on, "a forced true must mean scoped mode");
    assert!(!forced_off, "a forced false must mean safe mode");
    assert!(
        backend::version_is_scoped("agent-browser 0.43.1-hoo.1"),
        "fork versions carry -hoo."
    );
    assert!(
        !backend::version_is_scoped("agent-browser 0.43.0"),
        "upstream versions carry no -hoo."
    );
}

#[tokio::test]
async fn fake_safe_mode_closes_only_the_bound_tab() {
    // Safe mode is the default: the shared `tab list` shows every lease's
    // tabs, so a close removes only the bound target recorded at window
    // open — never the live `active` flag (the fake marks another lease's
    // tab active) and never other agents' targets. Listings show only the
    // lease's own bound entry, never other agents' URLs.
    let dir = test_tmp("safe-close");
    let argv_log = dir.join("argv.log");
    let fake = fake_wrapper(&dir, &argv_log, None, None);
    let (cdp, closed, _devtools) = fake_devtools().await;
    let (_helper, socket) = start_helper_cdp(&fake, &dir, &cdp).await;
    let peer = connect(&socket, ROOT_TOKEN).await;
    call(
        &peer,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("open");
    let list = call(&peer, "browser_tabs", serde_json::json!({"action": "list"}))
        .await
        .expect("list");
    let text = text_of(&list);
    assert!(
        text.contains("TGT-root-1"),
        "safe listing keeps the bound entry: {text}"
    );
    assert!(
        !text.contains("other-agent"),
        "safe listing never leaks other leases: {text}"
    );
    call(
        &peer,
        "browser_tabs",
        serde_json::json!({"action": "close", "tab": 1}),
    )
    .await
    .expect("close");
    let closed = closed.lock().unwrap().clone();
    assert_eq!(
        closed,
        vec!["/json/close/TGT-root-1"],
        "safe close takes only the bound target: {closed:?}"
    );
}

#[tokio::test]
async fn fake_scoped_mode_closes_every_listed_target() {
    // Scoped mode (fork): `tab list` is already owned-only, so listings
    // pass through and a close removes every listed target.
    let dir = test_tmp("scoped-close");
    let argv_log = dir.join("argv.log");
    let fake = fake_wrapper(&dir, &argv_log, None, None);
    let (cdp, closed, _devtools) = fake_devtools().await;
    let (helper, socket) = start_helper_cdp(&fake, &dir, &cdp).await;
    helper.set_scoped(true);
    let peer = connect(&socket, ROOT_TOKEN).await;
    call(
        &peer,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("open");
    let list = call(&peer, "browser_tabs", serde_json::json!({"action": "list"}))
        .await
        .expect("list");
    let text = text_of(&list);
    assert!(
        text.contains("other-agent"),
        "scoped listing passes the owned list through: {text}"
    );
    call(
        &peer,
        "browser_tabs",
        serde_json::json!({"action": "close", "tab": 1}),
    )
    .await
    .expect("close");
    let closed = closed.lock().unwrap().clone();
    assert_eq!(
        closed,
        vec!["/json/close/TGT-root-1", "/json/close/TGT-other"],
        "scoped close takes every listed target: {closed:?}"
    );
}

#[tokio::test]
async fn fake_browser_cli_tab_follows_the_mode() {
    // B3: `browser_cli tab` is refused in safe mode and allowed (scoped)
    // in scoped mode; other commands work in both.
    let dir = test_tmp("cli-tab-mode");
    let argv_log = dir.join("argv.log");
    let fake = fake_wrapper(&dir, &argv_log, None, None);
    let (helper, socket) = start_helper(&fake, &dir).await;
    let peer = connect(&socket, ROOT_TOKEN).await;
    let refused = call(
        &peer,
        "browser_cli",
        serde_json::json!({"args": ["tab", "list"]}),
    )
    .await;
    assert!(
        refused.is_err(),
        "safe mode refuses browser_cli tab list: {refused:?}"
    );
    call(
        &peer,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("open");
    let snapshot = call(
        &peer,
        "browser_cli",
        serde_json::json!({"args": ["snapshot"]}),
    )
    .await
    .expect("cli snapshot");
    assert!(
        !is_error(&snapshot),
        "other cli commands work in safe mode: {}",
        text_of(&snapshot)
    );
    helper.set_scoped(true);
    let listed = call(
        &peer,
        "browser_cli",
        serde_json::json!({"args": ["tab", "list"]}),
    )
    .await
    .expect("scoped cli tab list");
    assert!(
        !is_error(&listed) && text_of(&listed).contains("TGT-"),
        "scoped mode allows browser_cli tab list: {}",
        text_of(&listed)
    );
}

/// Spawn the real helper binary as a child (the way hoocode runs it),
/// with the mode forced via the child's own env (`HOO_BROWSER_AB_SCOPED`
/// `1` scoped, `0` safe). Never touches process env: parallel tests
/// share it. The fake Chromium prints the fake DevTools endpoint so
/// closes are observable.
async fn spawn_mode_helper(
    dir_tag: &str,
    ab_scoped: &str,
) -> (
    tokio::process::Child,
    PathBuf,
    PathBuf,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
    PathBuf,
) {
    let dir = test_tmp(dir_tag);
    let argv_log = dir.join("argv.log");
    let fake = fake_wrapper(&dir, &argv_log, None, None);
    let (cdp, closed, devtools) = fake_devtools().await;
    let chrome = dir.join("fake-chrome.sh");
    std::fs::write(
        &chrome,
        format!("#!/bin/bash\necho 'DevTools listening on {cdp}' >&2\nexec sleep 300\n"),
    )
    .expect("fake chrome");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&chrome, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let socket = dir.join("helper.sock");
    let helper_bin = PathBuf::from(env!("CARGO_BIN_EXE_hoo-browser"));
    let child = tokio::process::Command::new(&helper_bin)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .env("HOOCODE_HELPER_SOCKET", &socket)
        .env("HOOCODE_HELPER_TOKEN", ROOT_TOKEN)
        .env("HOOCODE_AGENT_ID", "root")
        .env("HOO_BROWSER_DATA", &dir)
        .env("HOO_BROWSER_AGENT_BROWSER", &fake)
        .env("HOO_BROWSER_CHROME", &chrome)
        .env("HOO_BROWSER_AB_SCOPED", ab_scoped)
        .env_remove("HOO_BROWSER_CALL_TIMEOUT_SECS")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn helper");
    wait_for_socket(&socket).await;
    (child, socket, argv_log, closed, devtools, dir)
}

/// Stop a spawned mode helper and its fake-Chromium tree (best effort):
/// SIGTERM first so the helper runs its fast shutdown, then SIGKILL,
/// then the browser group via the pid in the profile lock.
async fn stop_mode_helper(child: &mut tokio::process::Child, dir: &Path) {
    if let Some(pid) = child.id() {
        // SAFETY: SIGTERM to our own test child.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().expect("wait").is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
    let lock_text = std::fs::read_to_string(dir.join("profile.lock")).unwrap_or_default();
    if let Some(pid) = lock_text.lines().find_map(|line| {
        line.strip_prefix("chromium=")
            .and_then(|v| v.parse::<libc::pid_t>().ok())
    }) {
        // SAFETY: SIGKILL to the fake browser tree only.
        unsafe {
            if libc::getpgid(pid) == pid {
                let _ = libc::killpg(pid, libc::SIGKILL);
            } else {
                let _ = libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

#[tokio::test]
async fn fake_close_lists_targets_before_closing_the_session() {
    // H4 close path per mode (B3), through a spawned helper child with
    // the mode forced in its own env:
    // - scoped (`HOO_BROWSER_AB_SCOPED=1`): `tab list` is already
    //   owned-only, so a close lists the lease's targets first, closes
    //   every listed target via DevTools, and only then `close`s.
    // - safe (`=0`): the shared `tab list` shows every lease's tabs, so
    //   the close takes only the bound target recorded at window open —
    //   no list first (a wedged daemon must not skip the only close),
    //   then `close`s. The bound target goes through the fake DevTools
    //   endpoint the fake Chromium advertises.
    // Either way `tab close` refuses the last tab over --cdp, so it must
    // never be the close path.
    for (mode, flag) in [("scoped", "1"), ("safe", "0")] {
        let (mut child, socket, argv_log, closed, devtools, dir) =
            spawn_mode_helper(&format!("closetargets-{mode}"), flag).await;
        let root = connect(&socket, ROOT_TOKEN).await;
        send_started(&root, "root/task-1", "root", 1, "child-1-token").await;
        // Registration races the first connect: an unknown token is a
        // fast 401, so retry until the helper knows us.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let child_peer = loop {
            match try_connect(&socket, "child-1-token").await {
                Ok(peer) => break peer,
                Err(_) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "{mode}: child never registered"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        };
        let opened = loop {
            match call(
                &child_peer,
                "browser_open",
                serde_json::json!({"url": "https://x.example/"}),
            )
            .await
            {
                Ok(result) => break result,
                Err(e) if e.contains("unknown token") => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "{mode}: child lost its registration"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("{mode}: protocol error: {e}"),
            }
        };
        assert!(
            !is_error(&opened),
            "{mode}: open failed: {}",
            text_of(&opened)
        );
        // Open-time binding (safe) performs no DevTools close; clear any
        // hits so the assertions below see only the tab close.
        closed.lock().unwrap().clear();
        let before = argv_lines(&argv_log).len();
        let done = call(
            &child_peer,
            "browser_tabs",
            serde_json::json!({"action": "close", "tab": 1}),
        )
        .await
        .expect("{mode}: close tab 1");
        assert!(!is_error(&done), "{mode}: close failed: {}", text_of(&done));
        let calls: Vec<String> = argv_lines(&argv_log)[before..].to_vec();
        let close_at = calls
            .iter()
            .position(|l| l.contains("--session root_task-1-1") && l.trim_end().ends_with(" close"))
            .expect("{mode}: close must end with `close`: {calls:?}");
        if flag == "1" {
            let list_at = calls
                .iter()
                .position(|l| l.contains("--session root_task-1-1") && l.contains(" tab list"))
                .expect("{mode}: close must list the lease targets first: {calls:?}");
            assert!(
                list_at < close_at,
                "{mode}: tab list must precede close: {calls:?}"
            );
            let got = closed.lock().unwrap().clone();
            assert_eq!(
                got,
                vec![
                    "/json/close/TGT-root_task-1-1".to_owned(),
                    "/json/close/TGT-other".to_owned()
                ],
                "{mode}: scoped close takes every listed target: {got:?}"
            );
        } else {
            assert!(
                !calls.iter().any(|l| l.contains(" tab list")),
                "{mode}: safe close lists nothing before `close`: {calls:?}"
            );
            let got = closed.lock().unwrap().clone();
            assert_eq!(
                got,
                vec!["/json/close/TGT-root_task-1-1".to_owned()],
                "{mode}: safe close takes only the bound target: {got:?}"
            );
        }
        assert!(
            !calls.iter().any(|l| l.contains(" tab close")),
            "{mode}: `tab close` refuses the last tab, never use it: {calls:?}"
        );
        stop_mode_helper(&mut child, &dir).await;
        devtools.abort();
    }
}

#[tokio::test]
async fn fake_spawn_failure_is_tool_error() {
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
}

#[tokio::test]
async fn fake_call_timeout_is_tool_error() {
    // A hanging agent-browser trips the per-call timeout, lowered for the
    // test (a 45 s timeout test would be absurd). The override travels on
    // the spawned helper's own env: setting it process-wide (`set_var`)
    // would leak into parallel tests' in-process helpers and fail their
    // calls after 1 s. Sessions containing "slow" sleep 30 s in the
    // fake, so `window new` for the slow agent always trips it.
    let dir = test_tmp("timeout");
    let socket = dir.join("helper.sock");
    let helper_bin = PathBuf::from(env!("CARGO_BIN_EXE_hoo-browser"));
    let mut child = tokio::process::Command::new(&helper_bin)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .env("HOOCODE_HELPER_SOCKET", &socket)
        .env("HOOCODE_HELPER_TOKEN", ROOT_TOKEN)
        .env("HOOCODE_AGENT_ID", "root")
        .env("HOO_BROWSER_DATA", &dir)
        .env(
            "HOO_BROWSER_AGENT_BROWSER",
            manifest_dir().join("tests/fake-agent-browser.sh"),
        )
        .env("HOO_BROWSER_CHROME", fake_chrome(&dir))
        .env("HOO_BROWSER_CALL_TIMEOUT_SECS", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn helper");
    wait_for_socket(&socket).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    root.send_notification(rmcp::model::ClientNotification::CustomNotification(
        CustomNotification::new(
            "hoocode/agentStarted".to_owned(),
            Some(serde_json::json!({
                "agentId": "root/slowpoke",
                "parentId": "root",
                "depth": 1,
                "token": "slow-token",
            })),
        ),
    ))
    .await
    .expect("agentStarted");
    let slow = connect(&socket, "slow-token").await;
    // Registration races the first call: an unknown token is a fast
    // protocol refusal, so retry until the helper knows us.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let timed = loop {
        let result = call(
            &slow,
            "browser_open",
            serde_json::json!({"url": "https://x.example/"}),
        )
        .await;
        match result {
            Err(e) if e.contains("unknown token") => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "slow agent never registered"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("protocol error: {e}"),
            Ok(timed) => break timed,
        }
    };
    assert!(is_error(&timed), "timeout must be a tool error");
    assert!(
        text_of(&timed).contains("timed out"),
        "got: {}",
        text_of(&timed)
    );
    // SAFETY: SIGKILL to our own test child.
    unsafe {
        libc::kill(
            child.id().expect("helper pid") as libc::pid_t,
            libc::SIGKILL,
        );
    }
    let _ = child.wait().await;
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
    // Only `browser_open` creates tabs: the child opens its slot, then
    // other tools resolve it.
    let child_open = call(
        &child,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("protocol ok");
    assert!(!is_error(&child_open));
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
// Signals: SIGTERM/SIGINT run the same fast shutdown as sessionEnding.
// ---------------------------------------------------------------------------

/// Fake Chromium for the signal test: prints a DevTools endpoint line like
/// the real browser, then sleeps, so there is a real process tree in its own
/// process group for killpg to reap.
fn fake_chrome(dir: &Path) -> PathBuf {
    let path = dir.join("fake-chrome.sh");
    std::fs::write(
        &path,
        "#!/bin/bash\necho 'DevTools listening on ws://127.0.0.1:1/devtools/browser/fake' >&2\nexec sleep 300\n",
    )
    .expect("fake chrome");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path
}

/// True while any process of the group still exists (signal 0 checks).
#[cfg(unix)]
fn pgid_alive(pgid: libc::pid_t) -> bool {
    // SAFETY: signal 0 sends nothing.
    unsafe { libc::killpg(pgid, 0) == 0 }
}

#[tokio::test]
async fn sigterm_shuts_down_fast_like_session_ending() {
    // The real helper binary as a child, the way hoocode runs it: fake
    // agent-browser plus a fake Chromium with a real process tree.
    let dir = test_tmp("sigterm");
    let socket = dir.join("helper.sock");
    let helper_bin = PathBuf::from(env!("CARGO_BIN_EXE_hoo-browser"));
    let mut child = tokio::process::Command::new(&helper_bin)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .env("HOOCODE_HELPER_SOCKET", &socket)
        .env("HOOCODE_HELPER_TOKEN", ROOT_TOKEN)
        .env("HOOCODE_AGENT_ID", "root")
        .env("HOO_BROWSER_DATA", &dir)
        .env(
            "HOO_BROWSER_AGENT_BROWSER",
            manifest_dir().join("tests/fake-agent-browser.sh"),
        )
        .env("HOO_BROWSER_CHROME", fake_chrome(&dir))
        .env_remove("HOO_BROWSER_CALL_TIMEOUT_SECS")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn helper");
    let helper_pid = child.id().expect("helper pid") as libc::pid_t;
    wait_for_socket(&socket).await;
    // Chromium's pid is recorded in the profile lock file (note_pids).
    let lock_text = std::fs::read_to_string(dir.join("profile.lock")).expect("profile lock");
    let chrome_pid: libc::pid_t = lock_text
        .lines()
        .find_map(|line| line.strip_prefix("chromium=").and_then(|v| v.parse().ok()))
        .expect("chromium pid in lock");
    // SAFETY: getpgid only reads.
    let pgid = unsafe { libc::getpgid(chrome_pid) };
    assert!(
        pgid > 0 && pgid == chrome_pid,
        "chromium must lead its own process group"
    );
    assert!(pgid_alive(pgid), "chromium tree alive before the signal");
    // SAFETY: SIGTERM to our own test child.
    unsafe {
        libc::kill(helper_pid, libc::SIGTERM);
    }
    // Fast shutdown (N1): the group is gone, the socket is removed, and the
    // helper exits 0 (a plain stop is not a crash).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let exited = child.try_wait().expect("wait");
        if !pgid_alive(pgid) && !socket.exists() {
            match exited {
                Some(status) => {
                    assert!(status.success(), "helper must exit 0, got {status}");
                    return;
                }
                None => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        } else {
            assert!(
                std::time::Instant::now() < deadline,
                "SIGTERM shutdown stalled: group alive={}, socket exists={}",
                pgid_alive(pgid),
                socket.exists()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// The fake daemon pid the fake agent-browser wrote for `session`
/// (`<sock_dir>/<session>.pid`, like the real `<session>.pid`), waiting
/// for it to appear: `window new` spawns the daemon before answering.
async fn daemon_pid(dir: &Path, session: &str) -> u32 {
    let pid_file = hoo_browser::backend::socket_dir(dir).join(format!("{session}.pid"));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                if pid > 0 {
                    return pid;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "daemon pid file never appeared: {}",
            pid_file.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The daemon must be reaped by the shutdown under test, not left running.
async fn assert_daemon_dead(pid: u32, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if !pid_alive(pid) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: daemon {pid} still alive after shutdown"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait for a spawned helper child to exit (sessionEnding / signals stop
/// it; the caller already delivered the trigger).
async fn wait_helper_exit(
    child: &mut tokio::process::Child,
    what: &str,
) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: helper never exited"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The startup line names the agent-browser binary and the isolation mode.
fn assert_startup_line(dir: &Path, mode: &str) {
    let log = std::fs::read_to_string(dir.join("hoo-browser.log")).expect("hoo-browser.log");
    assert!(
        log.lines()
            .any(|line| line.starts_with("hoo-browser: agent-browser ")
                && line.contains(", mode ")
                && line.ends_with(&format!(", mode {mode}"))),
        "startup line with the mode missing, got:\n{log}"
    );
}

#[tokio::test]
async fn fake_daemons_die_on_session_ending() {
    // The live leak: after `hoocode -p` exited, an agent-browser daemon
    // (PPID 1) was still running while the helper and Chromium were gone.
    // sessionEnding must reap every daemon the helper started (pid files
    // in the socket dir) instead of leaving them for the 10-minute idle.
    let (mut child, socket, _argv, _closed, devtools, dir) =
        spawn_mode_helper("daemon-session-ending", "1").await;
    let root = connect(&socket, ROOT_TOKEN).await;
    let opened = call(
        &root,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("open");
    assert!(!is_error(&opened), "open failed: {}", text_of(&opened));
    let pid = daemon_pid(&dir, "root-1").await;
    assert!(
        pid_alive(pid),
        "fake daemon must be running before sessionEnding"
    );
    send_session_ending(&root).await;
    let status = wait_helper_exit(&mut child, "sessionEnding").await;
    assert!(status.success(), "helper must exit 0, got {status}");
    assert_daemon_dead(pid, "sessionEnding").await;
    assert_startup_line(&dir, "scoped");
    devtools.abort();
}

#[tokio::test]
async fn fake_daemons_die_on_sigterm() {
    // Same leak through the signal path: SIGTERM must run the same fast
    // shutdown (browser tree, capped lease closes, daemon reaps) and the
    // startup line must name safe mode when the binary is upstream.
    let (mut child, socket, _argv, _closed, devtools, dir) =
        spawn_mode_helper("daemon-sigterm", "0").await;
    let root = connect(&socket, ROOT_TOKEN).await;
    let opened = call(
        &root,
        "browser_open",
        serde_json::json!({"url": "https://x.example/"}),
    )
    .await
    .expect("open");
    assert!(!is_error(&opened), "open failed: {}", text_of(&opened));
    let pid = daemon_pid(&dir, "root-1").await;
    assert!(pid_alive(pid), "fake daemon must be running before SIGTERM");
    // SAFETY: SIGTERM to our own test child.
    unsafe {
        libc::kill(
            child.id().expect("helper pid") as libc::pid_t,
            libc::SIGTERM,
        );
    }
    let status = wait_helper_exit(&mut child, "SIGTERM").await;
    assert!(status.success(), "helper must exit 0, got {status}");
    assert_daemon_dead(pid, "SIGTERM").await;
    assert_startup_line(&dir, "safe");
    devtools.abort();
}

// ---------------------------------------------------------------------------
// Real backend: real agent-browser + headless Chromium, behind HOO_BROWSER_IT.
// ---------------------------------------------------------------------------

async fn real_backend() -> Option<PathBuf> {
    if std::env::var("HOO_BROWSER_IT").as_deref() != Ok("1") {
        return None;
    }
    // Explicit binary wins; otherwise the helper's own pinned download
    // (spec B3), verified by hash, so the suite runs without the env var.
    if let Ok(path) = std::env::var("HOO_BROWSER_AGENT_BROWSER") {
        let path = PathBuf::from(&path);
        assert!(
            path.is_file(),
            "agent-browser binary missing: {}",
            path.display()
        );
        return Some(path);
    }
    let dir = test_tmp("real-pin");
    Some(
        hoo_browser::backend::ensure_agent_browser(&dir)
            .await
            .expect("download the pinned agent-browser"),
    )
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

/// The known-transient daemon race: the CLI wrote the full request, then
/// the response read hit EAGAIN (os error 35, macOS) under a cold or
/// loaded daemon, reported as outcome_unknown — or a navigation burst
/// aborted the load (ERR_ABORTED). The helper never auto-retries these
/// (the command may have run); callers re-issue once like a user would.
fn transient_daemon_race(text: &str) -> bool {
    text.contains("os error 35") || text.contains("outcome_unknown") || text.contains("ERR_ABORTED")
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
/// Every DevTools page target as `(target id, url)`. The urls let the
/// real suite count one agent's pages exactly (child-3's `agent=2` query)
/// instead of hard-coding global counts that differ per close mode.
async fn cdp_targets(endpoint: &str) -> Vec<(String, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let bare = endpoint
        .trim_start_matches("ws://")
        .trim_start_matches("wss://");
    let host_port = bare.split('/').next().unwrap_or("").to_owned();
    // HTTP/1.1 framed by Content-Length: the DevTools server keep-alives
    // 1.1 connections despite Connection: close (read_to_end would stall
    // forever), and answers 1.0 with an empty reply. Bounded anyway: a
    // wedged browser must fail the test, not hang it.
    let body = tokio::time::timeout(Duration::from_secs(15), async {
        let mut stream = tokio::net::TcpStream::connect(&host_port)
            .await
            .expect("cdp http");
        let request =
            format!("GET /json/list HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = stream.read(&mut chunk).await.expect("read headers");
            if n == 0 {
                panic!("cdp closed before headers");
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > 65536 {
                panic!("cdp headers too large");
            }
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]);
        let len: usize = headers
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
            })
            .next()
            .flatten()
            .expect("cdp content-length");
        while buf.len() < header_end + len {
            let n = stream.read(&mut chunk).await.expect("read body");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        buf[header_end..(header_end + len).min(buf.len())].to_vec()
    })
    .await
    .expect("cdp /json timed out");
    serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|targets| targets.as_array().cloned())
        .map(|targets| {
            targets
                .iter()
                .filter(|t| t.get("type") == Some(&Value::String("page".into())))
                .filter_map(|t| {
                    Some((
                        t.get("id").and_then(Value::as_str)?.to_owned(),
                        t.get("url").and_then(Value::as_str)?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn cdp_page_targets(endpoint: &str) -> usize {
    cdp_targets(endpoint).await.len()
}

#[tokio::test]
async fn real_no_steal_suite() {
    let Some(agent_browser) = real_backend().await else {
        println!("skipping real backend (HOO_BROWSER_IT=1 not set)");
        return;
    };
    let dir = test_tmp("real");
    let chrome = hoo_browser::backend::locate_chromium().expect("chromium for IT");
    let chromium = hoo_browser::backend::launch_chromium(&chrome, &dir, &dir, false)
        .await
        .expect("launch chromium");
    let sock_dir = hoo_browser::backend::socket_dir(&dir);
    hoo_browser::exec::ensure_agent_browser_config(&sock_dir).expect("helper config");
    let endpoint = chromium.endpoint.clone();
    let helper = Helper::new(
        agent_browser.clone(),
        endpoint.clone(),
        dir.clone(),
        sock_dir,
        ROOT_TOKEN.into(),
        "root".into(),
    );
    // B3: the real suite runs in scoped mode only when forced
    // (HOO_BROWSER_AB_SCOPED=1); otherwise the upstream binary probes safe.
    // Remember which: the close-count assertions below differ per mode.
    let scoped = hoo_browser::backend::probe_ab_scoped(&agent_browser).await;
    helper.set_scoped(scoped);
    // Hand the browser to the helper, as production startup does, so the
    // teardown below kills the whole process group (killpg), not just the
    // main child that `kill_on_drop` would take.
    helper
        .set_chromium(chromium.child, chromium.profile_lock)
        .await;
    let socket = dir.join("helper.sock");
    let _ = std::fs::remove_file(&socket);
    tokio::spawn(hoo_browser::server::serve(helper.clone(), socket.clone()));
    wait_for_socket(&socket).await;

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
    let mut open = call(&root, "browser_open", serde_json::json!({"url": page}))
        .await
        .expect("open");
    // Cold start: the first command can meet a daemon that is not
    // accepting yet (EAGAIN os error 35, reported as outcome_unknown).
    // The helper never auto-retries those (the command may have run);
    // the test re-issues once like a user, and the warm daemon lands it.
    if is_error(&open) && transient_daemon_race(&text_of(&open)) {
        open = call(&root, "browser_open", serde_json::json!({"url": page}))
            .await
            .expect("open retry");
    }
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
                // A navigation burst can abort one load, and a cold or
                // loaded daemon can drop a response read (EAGAIN os error
                // 35, outcome_unknown); the helper reports both as tool
                // errors and never auto-retries, so a single re-issue
                // lands them.
                if let Ok(result) = &opened {
                    if is_error(result) && transient_daemon_race(&text_of(result)) {
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

    // H4: `browser_tabs close` closes the lease's windows too: the
    // DevTools /json endpoint loses at least one page target. A close
    // fans out to a `tab list` (capped at 15 s) plus one DevTools close
    // per target, so poll past that worst case instead of racing it.
    let pages_full = cdp_page_targets(&endpoint).await;
    let closed_tab = call(
        &children[0],
        "browser_tabs",
        serde_json::json!({"action": "close", "tab": 2}),
    )
    .await
    .expect("close tab");
    assert!(
        !is_error(&closed_tab),
        "browser_tabs close failed: {}",
        text_of(&closed_tab)
    );
    let mut pages_closed = pages_full;
    for _ in 0..150 {
        pages_closed = cdp_page_targets(&endpoint).await;
        if pages_closed < pages_full {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        pages_closed < pages_full,
        "browser_tabs close must close windows too: {pages_closed} vs {pages_full} page targets"
    );

    // Cross-agent access is refused over the CLI fallback too.
    let cross = call(
        &children[0],
        "browser_cli",
        serde_json::json!({"args": ["--session", "root-1", "snapshot"]}),
    )
    .await;
    assert!(cross.is_err(), "cross-agent CLI must be refused: {cross:?}");

    // agentEnded closes that agent's sessions and windows. The expected
    // counts are measured, not hard-coded, and the same assertions hold
    // in both modes:
    // - safe: each lease closes only its bound target (recorded at window
    //   open), so exactly child-3's own pages go.
    // - scoped: each lease closes every target in its `tab list` (the
    //   fork scopes it to owned tabs: its pages plus its background
    //   tabs), so the total drops by at least its pages.
    // Either way child-3's `agent=2` pages must all be gone, the total
    // must drop by at least that many, and the survivors' pages stay.
    // Like the close above, poll past the worst case: two leases close
    // sequentially, each with a 15 s `tab list` cap.
    let targets_before = cdp_targets(&endpoint).await;
    let pages_before = targets_before.len();
    assert!(pages_before >= 7, "leases must own pages: {pages_before}");
    let owned_before: Vec<&(String, String)> = targets_before
        .iter()
        .filter(|(_, url)| url.contains("agent=2&"))
        .collect();
    assert_eq!(
        owned_before.len(),
        2,
        "child-3 must own exactly its two tab pages, got: {targets_before:?}"
    );
    println!(
        "real suite: scoped={scoped} pages_before={pages_before} (child-3 owns {})",
        owned_before.len()
    );
    notify_ended(&helper, &root, "root/task-3", "child-3-token").await;
    let ended = call(&children[2], "browser_snapshot", serde_json::json!({})).await;
    assert!(ended.is_err(), "ended agent must be unknown: {ended:?}");
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let targets_after = loop {
        let targets = cdp_targets(&endpoint).await;
        let owned_left = targets
            .iter()
            .filter(|(_, url)| url.contains("agent=2&"))
            .count();
        if owned_left == 0 && targets.len() + owned_before.len() <= pages_before {
            break targets;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agentEnded must close windows too: {} child-3 pages left, {} vs {pages_before} page targets (scoped={scoped})",
            owned_left,
            targets.len(),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let pages_after = targets_after.len();
    assert!(
        pages_after + owned_before.len() <= pages_before,
        "agentEnded must close windows too: {pages_after} vs {pages_before} page targets (scoped={scoped})"
    );
    // The survivors keep their pages: no-steal means the ended agent's
    // closes never touch other leases' windows, in either mode.
    for (agent, want) in [("agent=0&", 1), ("agent=1&", 2)] {
        let kept = targets_after
            .iter()
            .filter(|(_, url)| url.contains(agent))
            .count();
        assert_eq!(
            kept, want,
            "surviving {agent} pages must remain, got: {targets_after:?}"
        );
    }

    report_overheads(&dir);

    // Teardown (H3): close every lease, kill the browser tree, then stop
    // the agent-browser daemons. A bare `close` only disconnects: daemons
    // would idle out 10 min later, stranding a process per run.
    helper.close_all_sessions().await;
    helper.shutdown().await;
    helper.stop_daemons().await;
    assert_no_leftover_processes(&dir, &agent_browser).await;
}

/// True while the pid exists (signal 0 checks: it sends nothing).
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 sends nothing.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// No stray processes from this run may remain: every agent-browser
/// daemon pid file in the run's socket dir must be dead, no process may
/// still run the run's agent-browser binary, and no Chromium may still
/// carry the run's profile dir.
async fn assert_no_leftover_processes(dir: &Path, agent_browser: &Path) {
    let sock_dir = hoo_browser::backend::socket_dir(dir);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let mut live = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&sock_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("pid") {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        if pid > 0 && pid_alive(pid) {
                            live.push(pid);
                        }
                    }
                }
            }
        }
        if live.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agent-browser daemons still alive after teardown: {live:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let running = |pattern: &str| {
        std::process::Command::new("pgrep")
            .args(["-f", pattern])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let daemons = running(&agent_browser.to_string_lossy());
    assert!(
        daemons.is_empty(),
        "agent-browser processes remain after teardown: {daemons}"
    );
    let chromes = running(&dir.to_string_lossy());
    assert!(
        chromes.is_empty(),
        "chromium processes remain after teardown: {chromes}"
    );
    // The daemon leak: no agent-browser process tied to this run's socket
    // dir may survive shutdown (daemons don't idle-exit while attached via
    // --cdp; their socket dir is the per-session runtime dir hoocode
    // removes).
    let with_sock_dir = running(&sock_dir.to_string_lossy());
    assert!(
        with_sock_dir.is_empty(),
        "agent-browser processes with the helper's socket dir remain after shutdown: {with_sock_dir}"
    );
}

fn report_overheads(dir: &Path) {
    let text = std::fs::read_to_string(dir.join("hoo-browser.log")).unwrap_or_default();
    let mut overheads: Vec<u128> = Vec::new();
    let mut windows: Vec<u128> = Vec::new();
    let mut children: Vec<u128> = Vec::new();
    let mut totals: Vec<u128> = Vec::new();
    for line in text.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if let Some(us) = value.get("overhead_us").and_then(Value::as_u64) {
                overheads.push(us as u128);
            }
            // Lines from before the window-new split have no such field.
            windows.push(
                value
                    .get("window_new_us")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u128,
            );
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
    windows.sort_unstable();
    children.sort_unstable();
    totals.sort_unstable();
    let pick = |v: &[u128], p: f64| v[((v.len() as f64 * p) as usize).min(v.len() - 1)];
    println!(
        "calls={} helper-only overhead_us p50={} p95={} max={} | window-new window_new_us p50={} p95={} | spawn+connect child_us p50={} p95={} | total_us p50={} p95={} (see scripts/overhead.py)",
        overheads.len(),
        pick(&overheads, 0.5),
        pick(&overheads, 0.95),
        overheads.last().unwrap(),
        pick(&windows, 0.5),
        pick(&windows, 0.95),
        pick(&children, 0.5),
        pick(&children, 0.95),
        pick(&totals, 0.5),
        pick(&totals, 0.95),
    );
}
