#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A `bash` call that outlives the turn's threshold: the turn stops waiting,
//! the command keeps running as a job, and its output reaches the session when
//! it ends. There is one job table, so `/jobs` lists it and `/jobs cancel`
//! stops it — and a turn's cancel does not.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{
    BlockId, MockBody, MockTransport, StopReason, StreamEvent, ToolCallRef, Transport,
};
use titi_tools::{ApprovalMode, ReadCache, SensitivePolicy, ToolRegistry};

struct MapResolver(HashMap<String, Arc<dyn Transport>>);

impl TransportResolver for MapResolver {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        self.0
            .get(model)
            .cloned()
            .map(|transport| ResolvedModel::without_credential(model, transport))
            .ok_or_else(|| RegistryError::UnknownModel(model.into()))
    }
}

fn resolver(transport: Arc<dyn Transport>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(
        [("primary".to_owned(), transport)].into_iter().collect(),
    ))
}

fn done() -> MockBody {
    MockBody::Events(vec![StreamEvent::Done {
        reason: StopReason::Stop,
    }])
}

/// One `bash` call, the way a model asks for it.
fn bash_call(call_id: &str, args: &str) -> MockBody {
    MockBody::Events(vec![
        StreamEvent::ToolcallStart {
            id: BlockId::new("tool"),
            call: ToolCallRef {
                call_id: call_id.into(),
                name: "bash".into(),
            },
        },
        StreamEvent::ToolcallDelta {
            id: BlockId::new("tool"),
            json: args.into(),
        },
        StreamEvent::ToolcallEnd {
            id: BlockId::new("tool"),
        },
        StreamEvent::Done {
            reason: StopReason::ToolUse,
        },
    ])
}

/// The session's tools, built the way `titi-cli` builds them: the real
/// workspace tools on the engine's cancel key, so the engine can install its
/// background door on `bash`.
fn workspace_registry(root: &std::path::Path, interrupt: titi_tools::Interrupt) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    for tool in titi_tools::workspace_tools_with_interrupt(
        root,
        ReadCache::default(),
        SensitivePolicy::default(),
        interrupt,
    ) {
        tools.register(Arc::from(tool));
    }
    tools
}

/// A session on `root` whose background threshold is `after`: tiny in these
/// tests so a `sleep 5` need not take five seconds. A real session reads
/// `TITI_BASH_BACKGROUND_MS`, then the tool's own default, for the same number.
fn config(root: &std::path::Path, after: Duration) -> EngineConfig {
    let mut config = EngineConfig::new("primary");
    config.approval_mode = ApprovalMode::Yolo;
    config.workspace_root = Some(root.to_path_buf());
    config.background_after = Some(after);
    config
}

/// Waits for the first event `pick` accepts, giving up rather than hanging.
async fn wait_for<T>(
    engine: &mut titi_engine::Engine,
    mut pick: impl FnMut(&EngineEvent) -> Option<T>,
) -> T {
    let deadline = Duration::from_secs(20);
    tokio::time::timeout(deadline, async {
        while let Some(event) = engine.recv().await {
            if let Some(found) = pick(&event) {
                return found;
            }
        }
        panic!("the engine stopped before the event arrived");
    })
    .await
    .expect("the event never arrived")
}

async fn submit(engine: &mut titi_engine::Engine, text: &str) {
    engine
        .send(EngineCommand::SubmitPrompt { text: text.into() })
        .await
        .unwrap();
}

/// What `/jobs` answers right now.
async fn job_ids(engine: &mut titi_engine::Engine) -> Vec<String> {
    engine.send(EngineCommand::ListJobs).await.unwrap();
    wait_for(engine, |event| match event {
        EngineEvent::JobList { jobs } => Some(jobs.iter().map(|job| job.id.to_string()).collect()),
        _ => None,
    })
    .await
}

/// A command past the threshold comes back at once, names the job the model is
/// told about, and that job is what `/jobs` lists. Cancelling it stops the
/// command: the file it would have written after 30 seconds never appears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_past_the_threshold_comes_back_as_a_listed_job() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("done");
    let transport = Arc::new(MockTransport::new(vec![
        bash_call(
            "call-1",
            &format!(
                r#"{{"command": "sleep 30 && echo done > {}"}}"#,
                marker.display()
            ),
        ),
        done(),
        // The report of a cancelled job still arrives as its own turn.
        done(),
    ]));
    let config = config(dir.path(), Duration::from_millis(200));
    let tools = workspace_registry(dir.path(), config.interrupt.clone());
    let mut engine =
        EngineRuntime::start_with_tools(config, resolver(Arc::clone(&transport) as _), tools);
    submit(&mut engine, "start the long one").await;

    let started = std::time::Instant::now();
    let output = wait_for(&mut engine, |event| match event {
        EngineEvent::ToolFinished { output, .. } => Some(output.to_string()),
        _ => None,
    })
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the turn waited on the command: {:?}",
        started.elapsed()
    );
    assert!(
        output.contains("moved to the background as bg-1"),
        "{output}"
    );
    assert!(
        output.contains("the output will arrive when it finishes"),
        "{output}"
    );
    assert_eq!(job_ids(&mut engine).await, ["bg-1"]);

    engine
        .send(EngineCommand::CancelJob {
            job_id: "bg-1".into(),
        })
        .await
        .unwrap();
    let stopped = wait_for(&mut engine, |event| match event {
        EngineEvent::JobFinished { job_id } => Some(job_id.to_string()),
        _ => None,
    })
    .await;
    assert_eq!(stopped, "bg-1", "the job's own end is what stops it");
    assert!(job_ids(&mut engine).await.is_empty());
    // The command's own 30 seconds: had the cancel not reached it, this file
    // would have appeared. It must not.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !marker.exists(),
        "the cancelled command kept running to its echo"
    );
}

/// What the command printed arrives afterwards, carrying how it exited, and a
/// print too large for the context window is bounded by the tool-output cap
/// rather than by a second one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_job_reports_its_output_and_exit_status_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        bash_call("call-1", r#"{"command": "sleep 1; seq 1 60000; exit 3"}"#),
        done(),
        done(),
    ]));
    let config = config(dir.path(), Duration::from_millis(200));
    let tools = workspace_registry(dir.path(), config.interrupt.clone());
    let mut engine =
        EngineRuntime::start_with_tools(config, resolver(Arc::clone(&transport) as _), tools);
    submit(&mut engine, "print a lot").await;

    // The turn that started the command ends while it still runs.
    wait_for(&mut engine, |event| match event {
        EngineEvent::TurnFinished { .. } => Some(()),
        _ => None,
    })
    .await;

    // Then the report arrives as a follow-up turn of its own.
    wait_for(&mut engine, |event| match event {
        EngineEvent::TurnFinished { .. } => Some(()),
        _ => None,
    })
    .await;

    let requests = transport.requests();
    let report = requests
        .last()
        .expect("the report turn was sent to the provider")
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .find(|content| content.contains("background job bg-1"))
        .expect("the report carries the job id")
        .to_owned();
    assert!(
        report.contains("background job bg-1 finished, exit 3"),
        "{report}"
    );
    assert!(
        report.contains("$ sleep 1; seq 1 60000; exit 3"),
        "{report}"
    );
    assert!(report.contains("1\n2\n3\n"), "the output arrives: {report}");
    assert!(
        report.contains("characters left out"),
        "the huge print was bounded rather than delivered whole: {} chars",
        report.chars().count()
    );
    assert!(
        report.chars().count() < titi_engine::tool_loop::MAX_TOOL_OUTPUT + 512,
        "the report spent {} characters",
        report.chars().count()
    );
}

/// The report goes through the same redactor a tool result does. A key the
/// command printed does not reach the session just because the command
/// outlived the turn that started it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reported_output_is_masked_like_a_tool_result() {
    let dir = tempfile::tempdir().unwrap();
    let secret = "sk-test-0000000000000000";
    let transport = Arc::new(MockTransport::new(vec![
        bash_call(
            "call-1",
            &format!(r#"{{"command": "sleep 1; echo TOKEN={secret}; exit 2"}}"#),
        ),
        done(),
        done(),
    ]));
    let config = config(dir.path(), Duration::from_millis(200));
    let tools = workspace_registry(dir.path(), config.interrupt.clone());
    let mut engine =
        EngineRuntime::start_with_tools(config, resolver(Arc::clone(&transport) as _), tools);
    submit(&mut engine, "print a token").await;
    wait_for(&mut engine, |event| match event {
        EngineEvent::TurnFinished { .. } => Some(()),
        _ => None,
    })
    .await;
    wait_for(&mut engine, |event| match event {
        EngineEvent::TurnFinished { .. } => Some(()),
        _ => None,
    })
    .await;

    let report = transport
        .requests()
        .iter()
        .flat_map(|request| request.messages.iter())
        .map(|message| message.content.as_str())
        .find(|content| content.contains("background job bg-1"))
        .expect("the report reached the session")
        .to_owned();
    assert!(report.contains("exit 2"), "{report}");
    assert!(
        report.contains("[redacted]"),
        "the key was not masked: {report}"
    );
    assert!(
        !report.contains(secret),
        "the key reached the session: {report}"
    );
}

/// A command that finishes inside the threshold is the same call it always
/// was: the plain answer, the exit line, and nothing in `/jobs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_under_the_threshold_is_the_same_result_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(MockTransport::new(vec![
        bash_call("call-1", r#"{"command": "echo out; echo err >&2; exit 3"}"#),
        done(),
    ]));
    let config = config(dir.path(), Duration::from_secs(30));
    let tools = workspace_registry(dir.path(), config.interrupt.clone());
    let mut engine =
        EngineRuntime::start_with_tools(config, resolver(Arc::clone(&transport) as _), tools);
    submit(&mut engine, "quick one").await;

    let (output, is_error) = wait_for(&mut engine, |event| match event {
        EngineEvent::ToolFinished {
            output, is_error, ..
        } => Some((output.to_string(), *is_error)),
        _ => None,
    })
    .await;
    assert_eq!(output, "exit 3\nout\nerr\n");
    assert!(is_error, "a failed command is still an error");
    assert!(job_ids(&mut engine).await.is_empty());
}

/// The turn's cancel kills the command the turn is waiting on — and leaves the
/// one it already handed to the background alone. The backgrounded command
/// writes its marker seconds after the cancel, so the file is the proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_cancel_leaves_the_backgrounded_command_alone() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("survived");
    // Three seconds: the first command is handed over at three, the second —
    // a one-second deadline, under the threshold — stays foreground and is
    // what the cancel kills.
    let transport = Arc::new(MockTransport::new(vec![
        bash_call(
            "call-1",
            &format!(r#"{{"command": "sleep 5 && touch {}"}}"#, marker.display()),
        ),
        bash_call("call-2", r#"{"command": "sleep 20", "timeout_secs": 1}"#),
        done(),
        done(),
    ]));
    let config = config(dir.path(), Duration::from_secs(3));
    let tools = workspace_registry(dir.path(), config.interrupt.clone());
    let mut engine =
        EngineRuntime::start_with_tools(config, resolver(Arc::clone(&transport) as _), tools);
    submit(&mut engine, "one long, one longer").await;

    // The first call is handed over; the second is still the turn's.
    let output = wait_for(&mut engine, |event| match event {
        EngineEvent::ToolFinished {
            call_id, output, ..
        } if call_id.as_str() == "call-1" => Some(output.to_string()),
        _ => None,
    })
    .await;
    assert!(
        output.contains("moved to the background as bg-1"),
        "{output}"
    );
    while let Some(event) = engine.recv().await {
        if matches!(
            event,
            EngineEvent::ToolStarted { call_id, .. } if call_id.as_str() == "call-2"
        ) {
            break;
        }
    }
    engine.send(EngineCommand::Cancel).await.unwrap();

    // The cancel reached the foreground command of the turn...
    let killed = wait_for(&mut engine, |event| match event {
        EngineEvent::ToolFinished {
            call_id, output, ..
        } if call_id.as_str() == "call-2" => Some(output.to_string()),
        _ => None,
    })
    .await;
    assert!(killed.contains("interrupted"), "{killed}");

    // ...and the backgrounded one runs its five seconds out, then reports.
    wait_for(&mut engine, |event| match event {
        EngineEvent::JobFinished { job_id } if job_id.as_str() == "bg-1" => Some(()),
        _ => None,
    })
    .await;
    assert!(
        marker.exists(),
        "the turn's cancel killed a command that was already in the background"
    );
}

/// The run driven by hand, with the threshold coming from the environment
/// alone — the config names none, exactly as a real session's does. Nothing
/// here is a mock but the model: the tools are the real workspace tools, the
/// commands are real processes, and the markers are real files.
///
/// `TITI_BASH_BACKGROUND_MS=300 cargo test -p titi-engine --locked \
///  --test background -- --ignored --nocapture`
///
/// Without a small threshold in the environment there is nothing to prove, so
/// it says so and returns instead of waiting a minute.
#[ignore = "run by hand with TITI_BASH_BACKGROUND_MS set to a small value"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_environment_sets_the_threshold_and_the_output_lands_in_the_session() {
    let threshold = std::env::var("TITI_BASH_BACKGROUND_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok());
    let Some(after) = threshold.filter(|after| *after <= 2_000) else {
        eprintln!("TITI_BASH_BACKGROUND_MS is not set to a small number; nothing to prove here");
        return;
    };
    eprintln!("threshold: {after}ms, from TITI_BASH_BACKGROUND_MS");

    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("late");
    let trapped = dir.path().join("trapped");
    let transport = Arc::new(MockTransport::new(vec![
        bash_call(
            "call-1",
            &format!(
                r#"{{"command": "sleep 3 && echo done | tee {}"}}"#,
                marker.display()
            ),
        ),
        done(),
        done(),
        bash_call(
            "call-2",
            &format!(
                r#"{{"command": "trap 'touch {}; exit 3' TERM; sleep 30"}}"#,
                trapped.display()
            ),
        ),
        done(),
        done(),
    ]));
    let mut config = EngineConfig::new("primary");
    config.approval_mode = ApprovalMode::Yolo;
    config.workspace_root = Some(dir.path().to_path_buf());
    // Deliberately none: the environment is what this run is proving.
    config.background_after = None;
    let tools = workspace_registry(dir.path(), config.interrupt.clone());
    let mut engine =
        EngineRuntime::start_with_tools(config, resolver(Arc::clone(&transport) as _), tools);

    // A command that runs three seconds comes back in a fraction of one.
    submit(&mut engine, "run the long one").await;
    let started = std::time::Instant::now();
    let output = wait_for(&mut engine, |event| match event {
        EngineEvent::ToolFinished {
            call_id, output, ..
        } if call_id.as_str() == "call-1" => Some(output.to_string()),
        _ => None,
    })
    .await;
    let handed_over = started.elapsed();
    eprintln!("the tool answered in {:?}: {}", handed_over, output.trim());
    assert!(
        handed_over < Duration::from_secs(2),
        "the call waited on a three-second command: {handed_over:?}"
    );
    assert!(
        output.contains("moved to the background as bg-1"),
        "{output}"
    );
    assert_eq!(job_ids(&mut engine).await, ["bg-1"]);

    // The marker is written after the turn gave up waiting, and the report
    // arrives as a follow-up turn with the exit status.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !marker.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("marker written after the turn ended: {}", marker.exists());
    assert!(marker.exists(), "the command did not run to completion");
    wait_for(&mut engine, |event| match event {
        EngineEvent::JobFinished { job_id } if job_id.as_str() == "bg-1" => Some(()),
        _ => None,
    })
    .await;
    wait_for(&mut engine, |event| match event {
        EngineEvent::TurnFinished { .. } => Some(()),
        _ => None,
    })
    .await;
    let requests = transport.requests();
    let report = requests
        .iter()
        .flat_map(|request| request.messages.iter())
        .map(|message| message.content.as_str())
        .find(|content| content.contains("background job bg-1"))
        .expect("the report reached the session")
        .to_owned();
    eprintln!("the session was told: {}", report.trim());
    assert!(
        report.contains("background job bg-1 finished, exit 0"),
        "{report}"
    );
    assert_eq!(
        report.lines().last(),
        Some("done"),
        "the command's own output is the report's tail: {report}"
    );

    // And a second one, cancelled by name: the trap it set runs, so the TERM
    // really reached the process group.
    submit(&mut engine, "run the trapper").await;
    let output = wait_for(&mut engine, |event| match event {
        EngineEvent::ToolFinished {
            call_id, output, ..
        } if call_id.as_str() == "call-2" => Some(output.to_string()),
        _ => None,
    })
    .await;
    assert!(
        output.contains("moved to the background as bg-2"),
        "{output}"
    );
    assert_eq!(job_ids(&mut engine).await, ["bg-2"]);
    engine
        .send(EngineCommand::CancelJob {
            job_id: "bg-2".into(),
        })
        .await
        .unwrap();
    wait_for(&mut engine, |event| match event {
        EngineEvent::JobFinished { job_id } if job_id.as_str() == "bg-2" => Some(()),
        _ => None,
    })
    .await;
    eprintln!("the job cancel stopped bg-2: {}", trapped.exists());
    assert!(trapped.exists(), "the group never got SIGTERM");
    assert!(job_ids(&mut engine).await.is_empty());
}
