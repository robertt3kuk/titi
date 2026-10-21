//! The client and manager against a real Streamable HTTP server process: the
//! fixture in `tests/fixtures/mcp_http_server.py` (MCP 2025-11-25, stdlib
//! `http.server` on a free loopback port).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use titi_config::mcp::McpServerDef;
use titi_mcp::{
    Client, EnvLookup, HttpLaunch, ManagerConfig, McpError, McpManager, ServerState, ServerStatus,
};
use titi_tools::ToolHandler;
use tokio::sync::mpsc::unbounded_channel;

struct Server {
    child: Child,
    port: u16,
    log: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Server {
    fn start(env: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("seen.jsonl");
        let mut command = Command::new("python3");
        command
            .arg(format!(
                "{}/tests/fixtures/mcp_http_server.py",
                env!("CARGO_MANIFEST_DIR")
            ))
            .env("FIXTURE_LOG", &log)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line.trim().strip_prefix("PORT ").unwrap().parse().unwrap();
        Self {
            child,
            port,
            log,
            _dir: dir,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// What the server saw, one entry per request.
    fn seen(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn rpcs(&self) -> Vec<String> {
        self.seen()
            .iter()
            .map(|entry| match entry["rpc"].as_str() {
                Some(rpc) => rpc.to_owned(),
                None => entry["http"].as_str().unwrap().to_owned(),
            })
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn launch(url: &str, headers: &[(&str, &str)]) -> HttpLaunch {
    HttpLaunch {
        url: url.to_owned(),
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        timeout: Duration::from_secs(10),
    }
}

async fn connect(server: &Server) -> Client {
    let (tx, _rx) = unbounded_channel();
    Client::connect_http(&launch(&server.url(), &[]), tx)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_list_and_call_over_a_json_body() {
    let server = Server::start(&[]);
    let client = connect(&server).await;
    assert_eq!(client.server_name, "fixture-http");
    assert_eq!(client.version, "2025-11-25");
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 6, "{tools:?}");
    let echoed = client
        .call_tool("echo", json!({ "text": "hello over http" }))
        .await
        .unwrap();
    assert_eq!(echoed.text, "hello over http");
    assert!(!echoed.is_error);
    let failed = client.call_tool("fail", json!({})).await.unwrap();
    assert!(failed.is_error);
    assert_eq!(
        server.rpcs(),
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call",
            "tools/call"
        ]
    );
    for entry in server.seen() {
        let accept = entry["headers"]["accept"].as_str().unwrap();
        assert!(accept.contains("application/json") && accept.contains("text/event-stream"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_protocol_version_follows_initialize_and_the_session_id_is_echoed_then_deleted() {
    let server = Server::start(&[("FIXTURE_SESSION", "1")]);
    let client = connect(&server).await;
    client.list_tools().await.unwrap();
    client.shutdown();
    let seen = server.seen();
    let rpcs = server.rpcs();
    assert_eq!(
        rpcs,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "DELETE"
        ]
    );
    let first = &seen[0]["headers"];
    assert!(
        first.get("mcp-session-id").is_none(),
        "initialize has no session yet"
    );
    assert!(first.get("mcp-protocol-version").is_none());
    let id = seen[1]["headers"]["mcp-session-id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(id.starts_with("sess-"));
    for entry in &seen[1..] {
        assert_eq!(entry["headers"]["mcp-session-id"], id.as_str(), "{entry}");
        assert_eq!(
            entry["headers"]["mcp-protocol-version"], "2025-11-25",
            "{entry}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_refuses_delete_is_not_an_error() {
    let server = Server::start(&[("FIXTURE_SESSION", "1"), ("FIXTURE_NO_DELETE", "1")]);
    let client = connect(&server).await;
    client.shutdown();
    assert_eq!(server.rpcs().last().map(String::as_str), Some("DELETE"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_stream_answer_is_read_like_a_json_one() {
    let server = Server::start(&[("FIXTURE_MODE", "sse"), ("FIXTURE_SESSION", "1")]);
    let client = connect(&server).await;
    assert_eq!(client.list_tools().await.unwrap().len(), 6);
    let sum = client
        .call_tool("add", json!({ "a": 2, "b": 3 }))
        .await
        .unwrap();
    assert_eq!(sum.text, "5");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_with_priming_comments_notifications_a_ping_and_multi_line_data() {
    let server = Server::start(&[("FIXTURE_MODE", "sse_multi"), ("FIXTURE_SESSION", "1")]);
    let client = connect(&server).await;
    // The reply is pretty-printed: its JSON spans several `data:` lines, CRLF.
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 6);
    let echoed = client
        .call_tool("echo", json!({ "text": "streamed" }))
        .await
        .unwrap();
    assert_eq!(echoed.text, "streamed");
    // The server's `ping` on the stream was answered with a POST of its own,
    // carrying the session.
    let seen = server.seen();
    let reply = seen
        .iter()
        .find(|entry| entry["rpc"] == "response srv-1")
        .unwrap_or_else(|| panic!("no reply to the ping: {:?}", server.rpcs()));
    assert_eq!(
        reply["headers"]["mcp-session-id"]
            .as_str()
            .map(|s| s.starts_with("sess-")),
        Some(true)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_the_server_never_closes_ends_with_the_response() {
    let server = Server::start(&[("FIXTURE_MODE", "sse"), ("FIXTURE_NO_STREAM_END", "1")]);
    let client = connect(&server).await;
    let started = Instant::now();
    assert_eq!(client.list_tools().await.unwrap().len(), 6);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_over_eight_mib_is_refused_in_both_modes_and_the_client_goes_on() {
    for mode in ["json", "sse"] {
        let server = Server::start(&[("FIXTURE_MODE", mode)]);
        let client = connect(&server).await;
        let big = 9 << 20;
        let error = client
            .call_tool("big", json!({ "size": big }))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, McpError::Protocol(why) if why.contains("larger than 8 MiB")),
            "{mode}: {error:?}"
        );
        let small = client
            .call_tool("big", json!({ "size": 1000 }))
            .await
            .unwrap();
        assert_eq!(small.text.len(), 1000, "{mode}: still usable");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_401_is_needs_auth() {
    let server = Server::start(&[("FIXTURE_AUTH", "Bearer right")]);
    let (tx, _rx) = unbounded_channel();
    let error = Client::connect_http(
        &launch(&server.url(), &[("Authorization", "Bearer wrong")]),
        tx,
    )
    .await
    .unwrap_err();
    assert_eq!(error, McpError::NeedsAuth);
    assert_eq!(error.to_string(), "needs auth (not supported yet)");
    // With the right header it connects: the header reaches the server.
    let (tx, _rx) = unbounded_channel();
    Client::connect_http(
        &launch(&server.url(), &[("Authorization", "Bearer right")]),
        tx,
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_is_not_followed_and_headers_do_not_leave_with_it() {
    let target = Server::start(&[]);
    let first = Server::start(&[("FIXTURE_REDIRECT", &target.url())]);
    let (tx, _rx) = unbounded_channel();
    let error = Client::connect_http(&launch(&first.url(), &[("X-Key", "secret")]), tx)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, McpError::Protocol(why) if why.contains("redirect")),
        "{error:?}"
    );
    assert!(
        target.seen().is_empty(),
        "nothing was sent on to the target"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_answer_times_out_at_the_servers_timeout() {
    let server = Server::start(&[]);
    let (tx, _rx) = unbounded_channel();
    let mut slow = launch(&server.url(), &[]);
    slow.timeout = Duration::from_secs(1);
    let client = Client::connect_http(&slow, tx).await.unwrap();
    let error = client
        .call_tool("slow", json!({ "seconds": 4 }))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        McpError::Timeout {
            method: "tools/call".to_owned(),
            secs: 1
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_server_names_the_host_and_never_the_query_or_a_header() {
    let (tx, _rx) = unbounded_channel();
    let error = Client::connect_http(
        &launch(
            "http://127.0.0.1:9/mcp?key=hunter2",
            &[("X-Key", "hunter2")],
        ),
        tx,
    )
    .await
    .unwrap_err();
    let text = error.to_string();
    assert!(matches!(error, McpError::Closed(_)), "{text}");
    assert!(
        text.contains("127.0.0.1:9") && !text.contains("hunter2"),
        "{text}"
    );
}

// ---- through the manager ----

struct Rig {
    manager: McpManager,
    _agent: tempfile::TempDir,
}

fn rig(vars: &[(&'static str, &'static str)]) -> Rig {
    let agent = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (tx, _events) = unbounded_channel();
    let vars: Vec<(&'static str, &'static str)> = vars.to_vec();
    let manager = McpManager::new(
        ManagerConfig {
            agent_dir: agent.path().to_owned(),
            workspace: work.path().to_owned(),
            inherit: Vec::new(),
            vars: EnvLookup::new(move |name| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }),
        },
        tx,
    );
    Rig {
        manager,
        _agent: agent,
    }
}

fn http_def(name: &str, url: &str, headers: &[(&str, &str)], project: bool) -> McpServerDef {
    McpServerDef {
        name: name.to_owned(),
        url: Some(url.to_owned()),
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        timeout_secs: Some(10),
        source: if project { ".mcp.json" } else { "mcp.json" }.to_owned(),
        project,
        ..McpServerDef::default()
    }
}

async fn until(rig: &Rig, what: &str, done: impl Fn(&[ServerStatus]) -> bool) -> Vec<ServerStatus> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = rig.manager.status();
        if done(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn state_of(status: &[ServerStatus], name: &str) -> ServerState {
    status
        .iter()
        .find(|server| server.name == name)
        .map(|server| server.state)
        .unwrap_or(ServerState::Failed)
}

async fn ready(rig: &Rig, name: &str) -> Vec<ServerStatus> {
    until(rig, "ready", |status| {
        state_of(status, name) == ServerState::Ready
    })
    .await
}

fn tool(rig: &Rig, name: &str) -> Arc<dyn ToolHandler> {
    rig.manager
        .handlers()
        .into_iter()
        .find(|handler| handler.definition().spec.name == name)
        .unwrap_or_else(|| panic!("no tool {name}: {:?}", rig.manager.status()))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_http_server_is_ready_with_its_tools_the_host_and_a_callable_echo() {
    let server = Server::start(&[("FIXTURE_SESSION", "1")]);
    let rig = rig(&[]);
    rig.manager
        .set_servers(vec![http_def("fixture", &server.url(), &[], false)]);
    let status = ready(&rig, "fixture").await;
    assert_eq!(status[0].tools.len(), 6);
    assert_eq!(status[0].host, format!("127.0.0.1:{}", server.port));
    assert_eq!(status[0].command, format!("http {}", server.url()));
    let result = tool(&rig, "mcp__fixture__echo")
        .invoke(json!({ "text": "hello from mcp" }))
        .await;
    assert_eq!(result.output, "hello from mcp");
    assert!(!result.is_error);
    // Leaving the manager ends the session.
    drop(rig);
    assert_eq!(server.rpcs().last().map(String::as_str), Some("DELETE"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_servers_variables_are_expanded_in_headers_and_url_and_a_missing_one_fails_it() {
    let server = Server::start(&[]);
    let rig = rig(&[
        ("TOKEN", "s3cret"),
        ("PORT", Box::leak(server.port.to_string().into_boxed_str())),
    ]);
    rig.manager.set_servers(vec![
        http_def(
            "fixture",
            "http://127.0.0.1:${PORT}/mcp",
            &[("X-Token", "Bearer ${TOKEN}")],
            false,
        ),
        http_def("missing", &server.url(), &[("X-Token", "${NOPE}")], false),
    ]);
    ready(&rig, "fixture").await;
    let status = until(&rig, "missing failed", |s| {
        state_of(s, "missing") == ServerState::Failed
    })
    .await;
    let missing = status.iter().find(|s| s.name == "missing").unwrap();
    assert!(
        missing.reason.contains("NOPE") && missing.reason.contains("not set"),
        "{}",
        missing.reason
    );
    let seen = tool(&rig, "mcp__fixture__headers").invoke(json!({})).await;
    assert_eq!(seen.output, r#"{"x-token": "Bearer s3cret"}"#);
    // Nothing was connected for the server whose variable is missing: every
    // request in the log carries the expanded token of the other one.
    assert!(
        server
            .seen()
            .iter()
            .all(|entry| entry["headers"]["x-token"] == "Bearer s3cret")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_server_gets_its_literal_headers_and_nothing_else_of_titis() {
    let server = Server::start(&[]);
    // Variables exist, and a project server still cannot name them.
    let rig = rig(&[("TOKEN", "s3cret"), ("OPENAI_API_KEY", "sk-test")]);
    rig.manager.set_servers(vec![http_def(
        "fixture",
        &server.url(),
        &[("X-Token", "Bearer ${TOKEN}")],
        true,
    )]);
    ready(&rig, "fixture").await;
    let seen = tool(&rig, "mcp__fixture__headers").invoke(json!({})).await;
    assert_eq!(
        seen.output, r#"{"x-token": "Bearer ${TOKEN}"}"#,
        "sent literally"
    );
    for entry in server.seen() {
        let names: Vec<&str> = entry["headers"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(
            !names.contains(&"authorization") && !names.contains(&"cookie"),
            "{names:?}"
        );
        let text = entry.to_string();
        assert!(
            !text.contains("s3cret") && !text.contains("sk-test"),
            "{text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_server_with_a_url_the_rule_refuses_is_never_connected() {
    let server = Server::start(&[]);
    let rig = rig(&[]);
    // An http URL that is not loopback (the loopback fixture is "elsewhere"
    // by name), and a URL that holds credentials.
    rig.manager.set_servers(vec![
        http_def("plain", "http://example.invalid/mcp", &[], true),
        http_def(
            "creds",
            &format!("http://127.0.0.1@example.invalid:{}/mcp", server.port),
            &[],
            true,
        ),
    ]);
    let status = until(&rig, "both failed", |s| {
        state_of(s, "plain") == ServerState::Failed && state_of(s, "creds") == ServerState::Failed
    })
    .await;
    assert!(status[0].reason.contains("user information") || status[0].reason.contains("https"));
    assert!(server.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_401_marks_the_server_failed_with_needs_auth() {
    let server = Server::start(&[("FIXTURE_AUTH", "Bearer right")]);
    let rig = rig(&[]);
    rig.manager
        .set_servers(vec![http_def("fixture", &server.url(), &[], false)]);
    let status = until(&rig, "failed", |s| {
        state_of(s, "fixture") == ServerState::Failed
    })
    .await;
    assert_eq!(status[0].reason, "needs auth (not supported yet)");
    // It is not retried as a crash: one initialize, nothing more.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.rpcs(), ["initialize"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ended_session_is_replaced_by_a_new_initialize() {
    let server = Server::start(&[("FIXTURE_SESSION", "1"), ("FIXTURE_EXPIRE_AFTER", "1")]);
    let rig = rig(&[]);
    rig.manager
        .set_servers(vec![http_def("fixture", &server.url(), &[], false)]);
    ready(&rig, "fixture").await;
    let echo = tool(&rig, "mcp__fixture__echo");
    assert_eq!(echo.invoke(json!({ "text": "one" })).await.output, "one");
    // The server dropped the session after that call: the next request gets a
    // 404 and says so.
    let second = echo.invoke(json!({ "text": "two" })).await;
    assert!(second.is_error, "{}", second.output);
    // The manager starts a new session, and the tool works again.
    let again = Instant::now();
    loop {
        let handler = rig
            .manager
            .handlers()
            .into_iter()
            .find(|h| h.definition().spec.name == "mcp__fixture__echo");
        if let Some(handler) = handler {
            let result = handler.invoke(json!({ "text": "three" })).await;
            if !result.is_error {
                assert_eq!(result.output, "three");
                break;
            }
        }
        assert!(again.elapsed() < Duration::from_secs(20), "never recovered");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let initializes = server.rpcs().iter().filter(|r| *r == "initialize").count();
    assert_eq!(initializes, 2);
}
