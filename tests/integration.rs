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

async fn notify_started(peer: &TestPeer, agent_id: &str, parent_id: &str, token: &str) {
    peer.send_notification(rmcp::model::ClientNotification::CustomNotification(
        CustomNotification::new(
            "hoocode/agentStarted".to_owned(),
            Some(serde_json::json!({
                "agentId": agent_id,
                "parentId": parent_id,
                "depth": 1,
                "token": token,
            })),
        ),
    ))
    .await
    .expect("agentStarted");
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn notify_ended(peer: &TestPeer, agent_id: &str) {
    peer.send_notification(rmcp::model::ClientNotification::CustomNotification(
        CustomNotification::new(
            "hoocode/agentEnded".to_owned(),
            Some(serde_json::json!({ "agentId": agent_id })),
        ),
    ))
    .await
    .expect("agentEnded");
    tokio::time::sleep(Duration::from_millis(200)).await;
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
    let (_helper, socket) =
        start_helper(&manifest_dir().join("tests/fake-agent-browser.sh"), &dir).await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started(&root, "root/task-1", "root", "child-1-token").await;
    notify_started(&root, "root/task-2", "root", "child-2-token").await;
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
    notify_started(&child1, "root/task-9", "root", "rogue-token").await;
    let rogue = try_connect(&socket, "rogue-token").await;
    assert!(rogue.is_err(), "rogue token must be unknown: {rogue:?}");

    // agentEnded closes that agent's sessions and drops its token.
    notify_ended(&root, "root/task-2").await;
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
    notify_ended(&root, "root/task-1").await;
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
                if let Some(ms) = value.get("overhead_ms").and_then(Value::as_u64) {
                    overheads.push(ms as u128);
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
        "overhead_ms over {} calls: p50={} p95={} max={}",
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

#[tokio::test]
async fn real_no_steal_suite() {
    let Some(agent_browser) = real_backend() else {
        println!("skipping real backend (HOO_BROWSER_IT=1 not set)");
        return;
    };
    let dir = test_tmp("real");
    let chrome = hoo_browser::backend::locate_chromium().expect("chromium for IT");
    let chromium = hoo_browser::backend::launch_chromium(&chrome, &dir, false)
        .await
        .expect("launch chromium");
    let helper = Helper::new(
        agent_browser,
        chromium.endpoint.clone(),
        dir.clone(),
        ROOT_TOKEN.into(),
        "root".into(),
    );
    let socket = dir.join("helper.sock");
    let _ = std::fs::remove_file(&socket);
    tokio::spawn(hoo_browser::server::serve(helper, socket.clone()));
    wait_for_socket(&socket).await;
    // Chromium dies with the test process (kill_on_drop in the handle).
    let _chromium = chromium;

    let (page, _page_server) = start_test_page().await;
    let root = connect(&socket, ROOT_TOKEN).await;
    notify_started(&root, "root/task-1", "root", "child-1-token").await;
    notify_started(&root, "root/task-2", "root", "child-2-token").await;
    notify_started(&root, "root/task-3", "root", "child-3-token").await;
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
                (i, tab, opened, snap)
            }));
        }
    }
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.expect("parallel task"));
    }
    for (i, tab, opened, snap) in results {
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
    }

    // Cross-agent access is refused over the CLI fallback too.
    let cross = call(
        &children[0],
        "browser_cli",
        serde_json::json!({"args": ["--session", "root-1", "snapshot"]}),
    )
    .await;
    assert!(cross.is_err(), "cross-agent CLI must be refused: {cross:?}");

    // agentEnded closes that agent's sessions.
    notify_ended(&root, "root/task-3").await;
    let ended = call(&children[2], "browser_snapshot", serde_json::json!({})).await;
    assert!(ended.is_err(), "ended agent must be unknown: {ended:?}");

    report_overheads(&dir);
    // Chromium is killed by `_chromium` drop at test end.
}

fn report_overheads(dir: &Path) {
    let text = std::fs::read_to_string(dir.join("hoo-browser.log")).unwrap_or_default();
    let mut overheads: Vec<u128> = Vec::new();
    let mut totals: Vec<u128> = Vec::new();
    for line in text.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if let Some(ms) = value.get("overhead_ms").and_then(Value::as_u64) {
                overheads.push(ms as u128);
            }
            if let Some(ms) = value.get("total_ms").and_then(Value::as_u64) {
                totals.push(ms as u128);
            }
        }
    }
    if overheads.is_empty() {
        println!("no overhead lines logged");
        return;
    }
    overheads.sort_unstable();
    totals.sort_unstable();
    let pick = |v: &[u128], p: f64| v[((v.len() as f64 * p) as usize).min(v.len() - 1)];
    println!(
        "overhead_ms over {} calls: p50={} p95={} max={} | total_ms p50={} p95={}",
        overheads.len(),
        pick(&overheads, 0.5),
        pick(&overheads, 0.95),
        overheads.last().unwrap(),
        pick(&totals, 0.5),
        pick(&totals, 0.95),
    );
}
