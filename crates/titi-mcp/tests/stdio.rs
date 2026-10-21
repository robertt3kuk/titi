//! The client and manager against a real server process: the fixture in
//! `tests/fixtures/mcp_server.py` speaks protocol 2025-11-25 over stdio.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use titi_config::mcp::{McpApproval, McpServerDef};
use titi_mcp::{
    ManagerConfig, McpEvent, McpManager, ServerState, ServerStatus, log_path, tool_name,
};
use titi_tools::{ApprovalTier, ToolHandler};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn fixture() -> String {
    format!(
        "{}/tests/fixtures/mcp_server.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn def(name: &str, env: &[(&str, &str)]) -> McpServerDef {
    McpServerDef {
        name: name.to_owned(),
        command: "python3".to_owned(),
        args: vec![fixture()],
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        cwd: None,
        disabled: false,
        timeout_secs: Some(10),
        approval: None,
        source: "test".to_owned(),
    }
}

struct Rig {
    agent: tempfile::TempDir,
    work: tempfile::TempDir,
    manager: McpManager,
    events: UnboundedReceiver<McpEvent>,
}

fn rig() -> Rig {
    let agent = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (tx, events) = unbounded_channel();
    // The allow-list is applied to this "process environment", so a variable
    // the list does not name must never reach a server.
    let inherit = titi_tools::hook_environment(
        [
            ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
            ("SECRET_FROM_PARENT".into(), "leaked".into()),
        ],
        work.path(),
    );
    let manager = McpManager::new(
        ManagerConfig {
            agent_dir: agent.path().to_owned(),
            workspace: work.path().to_owned(),
            inherit,
        },
        tx,
    );
    Rig {
        agent,
        work,
        manager,
        events,
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

fn tool(rig: &Rig, name: &str) -> Arc<dyn ToolHandler> {
    rig.manager
        .handlers()
        .into_iter()
        .find(|handler| handler.definition().spec.name == name)
        .unwrap_or_else(|| panic!("no tool {name}: {:?}", rig.manager.status()))
}

async fn ready(rig: &Rig, name: &str) {
    until(rig, "ready", |status| {
        state_of(status, name) == ServerState::Ready
    })
    .await;
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_handshake_runs_in_order_and_the_tools_are_listed() {
    let rig = rig();
    let log = rig.work.path().join("seen.log");
    rig.manager.set_servers(vec![def(
        "fixture",
        &[("FIXTURE_LOG", log.to_str().unwrap())],
    )]);
    ready(&rig, "fixture").await;
    assert_eq!(
        lines(&log),
        ["initialize 1", "notifications/initialized", "tools/list 2"]
    );
    let status = rig.manager.status();
    assert_eq!(status[0].command, format!("python3 {}", fixture()));
    assert_eq!(status[0].tools.len(), 12, "{:?}", status[0].tools);
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_register_as_mcp_server_tool_with_the_servers_schema_and_exec_approval() {
    let rig = rig();
    rig.manager.set_servers(vec![def("fixture", &[])]);
    ready(&rig, "fixture").await;
    let mut names: Vec<String> = rig
        .manager
        .handlers()
        .iter()
        .map(|handler| handler.definition().spec.name.to_string())
        .collect();
    names.sort();
    assert!(
        names.contains(&"mcp__fixture__echo".to_owned()),
        "{names:?}"
    );
    // A name providers would reject is made safe, and still reaches the tool.
    assert!(
        names.contains(&"mcp__fixture__dotted_name".to_owned()),
        "{names:?}"
    );
    let echo = tool(&rig, "mcp__fixture__echo").definition();
    assert_eq!(echo.approval, ApprovalTier::Exec);
    assert_eq!(echo.spec.description, "Echo the text back");
    assert_eq!(echo.spec.parameters["required"], json!(["text"]));
    let dotted = tool(&rig, "mcp__fixture__dotted_name");
    assert_eq!(dotted.invoke(json!({})).await.output, "dotted");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_set_approval_lowers_the_tier() {
    let rig = rig();
    let mut lowered = def("low", &[]);
    lowered.approval = Some(McpApproval::Read);
    let mut middle = def("mid", &[]);
    middle.approval = Some(McpApproval::Write);
    rig.manager
        .set_servers(vec![lowered, middle, def("plain", &[])]);
    for name in ["low", "mid", "plain"] {
        ready(&rig, name).await;
    }
    let tier = |name: &str| tool(&rig, name).definition().approval;
    assert_eq!(tier("mcp__low__echo"), ApprovalTier::Read);
    assert_eq!(tier("mcp__mid__echo"), ApprovalTier::Write);
    assert_eq!(tier("mcp__plain__echo"), ApprovalTier::Exec);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_round_trips_and_pages_are_followed() {
    let rig = rig();
    rig.manager
        .set_servers(vec![def("fixture", &[("FIXTURE_PAGES", "1")])]);
    ready(&rig, "fixture").await;
    assert_eq!(rig.manager.status()[0].tools.len(), 12, "both pages");
    let echo = tool(&rig, "mcp__fixture__echo");
    let out = echo.invoke(json!({ "text": "héllo" })).await;
    assert_eq!((out.output.as_str(), out.is_error), ("héllo", false));
    let add = tool(&rig, "mcp__fixture__add")
        .invoke(json!({ "a": 2, "b": 3 }))
        .await;
    assert_eq!(add.output, "5");
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_map_to_tool_errors_and_content_is_summarised() {
    let rig = rig();
    rig.manager.set_servers(vec![def("fixture", &[])]);
    ready(&rig, "fixture").await;
    // The tool ran and failed: its own text, flagged.
    let failed = tool(&rig, "mcp__fixture__fail").invoke(json!({})).await;
    assert!(failed.is_error);
    assert_eq!(failed.output, "it went wrong");
    // The server refused the request: the JSON-RPC error, named.
    let refused = tool(&rig, "mcp__fixture__rpc_error")
        .invoke(json!({}))
        .await;
    assert!(refused.is_error);
    assert!(
        refused.output.contains("error -32602: bad arguments"),
        "{}",
        refused.output
    );
    assert!(refused.output.contains("fixture"), "{}", refused.output);
    // Arguments the schema rules out never leave the process.
    let echo = tool(&rig, "mcp__fixture__echo");
    assert!(echo.refusal(&json!({})).unwrap().contains("text"));
    assert!(echo.refusal(&json!("x")).is_some());
    assert!(echo.invoke(json!({})).await.is_error);
    // Images and resources are named, not dumped.
    let mixed = tool(&rig, "mcp__fixture__image").invoke(json!({})).await;
    assert_eq!(
        mixed.output,
        "here\n[image]\n[resource file:///tmp/x.txt]\n[resource file:///tmp/y.txt]"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_call_times_out_and_the_server_is_told_to_stop() {
    let rig = rig();
    let log = rig.work.path().join("seen.log");
    let mut slow = def("fixture", &[("FIXTURE_LOG", log.to_str().unwrap())]);
    slow.timeout_secs = Some(1);
    rig.manager.set_servers(vec![slow]);
    ready(&rig, "fixture").await;
    let started = Instant::now();
    let out = tool(&rig, "mcp__fixture__slow")
        .invoke(json!({ "seconds": 3 }))
        .await;
    assert!(out.is_error);
    assert!(out.output.contains("no answer in 1s"), "{}", out.output);
    assert!(started.elapsed() < Duration::from_millis(2500));
    // The fixture is asleep until its 3 s are up, then reads the cancel.
    let deadline = Instant::now() + Duration::from_secs(6);
    while !lines(&log)
        .iter()
        .any(|line| line == "notifications/cancelled")
    {
        assert!(Instant::now() < deadline, "{:?}", lines(&log));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_a_call_cancels_it_on_the_server() {
    let rig = rig();
    let log = rig.work.path().join("seen.log");
    rig.manager.set_servers(vec![def(
        "fixture",
        &[("FIXTURE_LOG", log.to_str().unwrap())],
    )]);
    ready(&rig, "fixture").await;
    let slow = tool(&rig, "mcp__fixture__slow");
    let call = slow.invoke(json!({ "seconds": 2 }));
    // A cancelled turn drops the tool's future.
    let _ = tokio::time::timeout(Duration::from_millis(200), call).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !lines(&log)
        .iter()
        .any(|line| line == "notifications/cancelled")
    {
        assert!(Instant::now() < deadline, "{:?}", lines(&log));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_server_is_restarted_once_and_then_given_up_on() {
    let rig = rig();
    rig.manager.set_servers(vec![def("fixture", &[])]);
    ready(&rig, "fixture").await;
    let before = tool(&rig, "mcp__fixture__pid")
        .invoke(json!({}))
        .await
        .output;

    // The call that kills it is answered with an error, not a hang.
    let crashed = tool(&rig, "mcp__fixture__crash").invoke(json!({})).await;
    assert!(crashed.is_error, "{}", crashed.output);
    assert!(
        crashed.output.contains("exit status: 7"),
        "{}",
        crashed.output
    );
    // Restarted: ready again, as a different process.
    let deadline = Instant::now() + Duration::from_secs(15);
    let after = loop {
        let handlers = rig.manager.handlers();
        if let Some(pid) = handlers
            .iter()
            .find(|handler| handler.definition().spec.name == "mcp__fixture__pid")
        {
            let out = pid.invoke(json!({})).await;
            if !out.is_error {
                break out.output.to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "never restarted: {:?}",
            rig.manager.status()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_ne!(before, after);

    // The second crash is not restarted.
    let _ = tool(&rig, "mcp__fixture__crash").invoke(json!({})).await;
    let status = until(&rig, "failed", |status| {
        state_of(status, "fixture") == ServerState::Failed
    })
    .await;
    assert!(
        status[0].reason.contains("exit status: 7"),
        "{}",
        status[0].reason
    );
    assert!(
        status[0].reason.contains("/mcp restart fixture"),
        "{}",
        status[0].reason
    );
    assert!(
        rig.manager.handlers().is_empty(),
        "a failed server offers no tools"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_dies_while_starting_is_retried_once_and_its_stderr_is_in_the_reason() {
    let rig = rig();
    let marker = rig.work.path().join("crashed-once");
    rig.manager.set_servers(vec![def(
        "fixture",
        &[("FIXTURE_CRASH_ONCE", marker.to_str().unwrap())],
    )]);
    ready(&rig, "fixture").await;
    assert!(marker.exists());

    let rig2 = self::rig();
    rig2.manager
        .set_servers(vec![def("gone", &[("FIXTURE_EXIT_AT_START", "1")])]);
    let status = until(&rig2, "failed", |status| {
        state_of(status, "gone") == ServerState::Failed
    })
    .await;
    assert!(
        status[0].reason.contains("exit status: 3"),
        "{}",
        status[0].reason
    );
    assert!(
        status[0]
            .reason
            .contains("stderr: fixture: refusing to start"),
        "{}",
        status[0].reason
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_never_answers_the_handshake_fails_at_the_timeout() {
    let rig = rig();
    let mut hung = def("hung", &[("FIXTURE_NEVER_ANSWER", "1")]);
    hung.timeout_secs = Some(1);
    rig.manager.set_servers(vec![hung]);
    let status = until(&rig, "failed", |status| {
        state_of(status, "hung") == ServerState::Failed
    })
    .await;
    assert!(
        status[0].reason.contains("initialize got no answer in 1s"),
        "{}",
        status[0].reason
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_protocol_version_and_a_missing_program_are_failures_with_a_reason() {
    let rig = rig();
    let mut missing = def("missing", &[]);
    missing.command = "titi-no-such-program".to_owned();
    rig.manager.set_servers(vec![
        def("old", &[("FIXTURE_VERSION", "1999-01-01")]),
        missing,
    ]);
    let status = until(&rig, "both failed", |status| {
        state_of(status, "old") == ServerState::Failed
            && state_of(status, "missing") == ServerState::Failed
    })
    .await;
    let reason = |name: &str| {
        status
            .iter()
            .find(|server| server.name == name)
            .unwrap()
            .reason
            .clone()
    };
    assert!(reason("old").contains("1999-01-01"), "{}", reason("old"));
    assert!(
        reason("missing").contains("could not start"),
        "{}",
        reason("missing")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tools_list_changed_notification_refreshes_the_tools() {
    let mut rig = rig();
    rig.manager.set_servers(vec![def("fixture", &[])]);
    ready(&rig, "fixture").await;
    assert!(
        !rig.manager
            .handlers()
            .iter()
            .any(|h| h.definition().spec.name == "mcp__fixture__extra")
    );
    while rig.events.try_recv().is_ok() {}
    tool(&rig, "mcp__fixture__grow").invoke(json!({})).await;
    until(&rig, "the new tool", |status| {
        status[0]
            .tools
            .iter()
            .any(|tool| tool.name == "mcp__fixture__extra")
    })
    .await;
    assert_eq!(rig.events.recv().await, Some(McpEvent::Changed));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_gets_its_own_env_and_the_workspace_and_nothing_else_of_ours() {
    let rig = rig();
    let mut server = def("fixture", &[("FIXTURE_TOKEN", "tok")]);
    server.cwd = Some("sub".to_owned());
    std::fs::create_dir_all(rig.work.path().join("sub")).unwrap();
    rig.manager.set_servers(vec![server]);
    ready(&rig, "fixture").await;
    let out = tool(&rig, "mcp__fixture__whoami").invoke(json!({})).await;
    let (token, rest) = out.output.split_once('|').unwrap();
    let (secret, cwd) = rest.split_once('|').unwrap();
    assert_eq!(token, "tok");
    assert_eq!(
        secret, "-",
        "an env var outside the allow-list reached the server"
    );
    let want = std::fs::canonicalize(rig.work.path().join("sub")).unwrap();
    assert_eq!(std::fs::canonicalize(cwd).unwrap(), want);
}

#[tokio::test(flavor = "multi_thread")]
async fn standard_error_goes_to_a_per_server_log_under_the_agent_dir() {
    let rig = rig();
    rig.manager.set_servers(vec![def("fixture", &[])]);
    ready(&rig, "fixture").await;
    let log = log_path(rig.agent.path(), "fixture");
    assert_eq!(log, rig.agent.path().join("mcp/fixture.log"));
    assert!(
        lines(&log).contains(&"fixture: started".to_owned()),
        "{:?}",
        lines(&log)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_kills_the_server_and_what_it_started() {
    let rig = rig();
    let child_file = rig.work.path().join("child.pid");
    rig.manager.set_servers(vec![def(
        "fixture",
        &[("FIXTURE_SPAWN_CHILD", child_file.to_str().unwrap())],
    )]);
    ready(&rig, "fixture").await;
    let server = tool(&rig, "mcp__fixture__pid")
        .invoke(json!({}))
        .await
        .output
        .to_string();
    let helper = std::fs::read_to_string(&child_file).unwrap();
    assert!(alive(&server) && alive(&helper));
    rig.manager.shutdown();
    // Reaped by the time shutdown returns, except for what init is still
    // collecting.
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(&server) || alive(&helper) {
        assert!(
            Instant::now() < deadline,
            "server {server} / helper {helper} still alive"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(rig.manager.status().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn set_servers_keeps_unchanged_servers_stops_removed_ones_and_restart_starts_over() {
    let rig = rig();
    let a = def("a", &[]);
    rig.manager.set_servers(vec![a.clone(), def("b", &[])]);
    ready(&rig, "a").await;
    ready(&rig, "b").await;
    let first = tool(&rig, "mcp__a__pid")
        .invoke(json!({}))
        .await
        .output
        .to_string();
    let b_pid = tool(&rig, "mcp__b__pid")
        .invoke(json!({}))
        .await
        .output
        .to_string();

    rig.manager.set_servers(vec![a]);
    assert_eq!(rig.manager.status().len(), 1, "b is gone");
    assert_eq!(
        tool(&rig, "mcp__a__pid").invoke(json!({})).await.output,
        first,
        "an unchanged server is left running"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(&b_pid) {
        assert!(
            Instant::now() < deadline,
            "b's process outlived its removal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(rig.manager.restart("a"));
    assert!(!rig.manager.restart("nobody"));
    ready(&rig, "a").await;
    assert_ne!(
        tool(&rig, "mcp__a__pid").invoke(json!({})).await.output,
        first
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(&first) {
        assert!(
            Instant::now() < deadline,
            "the restarted server's old process lives"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_server_is_listed_and_never_started() {
    let rig = rig();
    let mut off = def("off", &[]);
    off.disabled = true;
    rig.manager.set_servers(vec![off]);
    assert_eq!(rig.manager.status()[0].state, ServerState::Disabled);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !log_path(rig.agent.path(), "off").exists(),
        "it was spawned"
    );
    assert!(rig.manager.handlers().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_client_answers_the_servers_ping_and_can_ping_it() {
    use titi_mcp::{Client, ClientEvent, Launch};
    let (tx, _events) = unbounded_channel::<ClientEvent>();
    let launch = Launch {
        command: "python3".to_owned(),
        args: vec![fixture()],
        env: titi_tools::hook_environment(
            [("PATH".into(), std::env::var_os("PATH").unwrap_or_default())],
            Path::new("."),
        ),
        cwd: PathBuf::from("."),
        stderr_log: None,
        timeout: Duration::from_secs(5),
    };
    let client = Client::connect(&launch, tx).await.unwrap();
    assert_eq!(client.version, "2025-11-25");
    assert_eq!(client.server_name, "fixture");
    assert!(client.has_tools);
    client.ping().await.unwrap();
    assert!(client.is_alive());
    client.shutdown();
    assert!(
        client.ping().await.is_err(),
        "a stopped server answers nothing"
    );
}

#[test]
fn tool_names_are_claude_shaped_safe_and_bounded() {
    assert_eq!(tool_name("fs", "read_file"), "mcp__fs__read_file");
    assert_eq!(tool_name("fs", "a.b c"), "mcp__fs__a_b_c");
    let long_a = tool_name("fs", &"x".repeat(100));
    let long_b = tool_name("fs", &format!("{}y", "x".repeat(99)));
    assert!(long_a.len() <= 64 && long_b.len() <= 64);
    assert_ne!(long_a, long_b, "two long names stay two names");
    assert!(long_a.starts_with("mcp__fs__xxx"));
}

#[test]
fn content_blocks_become_text() {
    use titi_mcp::render_content;
    assert_eq!(render_content(&json!({ "content": [] })), "");
    assert_eq!(
        render_content(&json!({ "content": [], "structuredContent": { "a": 1 } })),
        r#"{"a":1}"#
    );
    assert_eq!(
        render_content(&json!({ "content": [
            { "type": "text", "text": "a" },
            { "type": "audio", "data": "x", "mimeType": "audio/wav" },
            { "type": "future" },
        ] })),
        "a\n[audio]\n[future]"
    );
}
