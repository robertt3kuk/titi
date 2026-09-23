//! Background loops: the engine owns the timer, the surface only asks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use titi_engine::{
    EngineCommand, EngineConfig, EngineEvent, EngineRuntime, RegistryError, ResolvedModel,
    TransportResolver,
};
use titi_providers::{MockBody, MockTransport, StopReason, StreamEvent, Transport};

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

fn resolver(model: &str, transport: Arc<dyn Transport>) -> Arc<dyn TransportResolver> {
    Arc::new(MapResolver(HashMap::from([(model.to_owned(), transport)])))
}

fn done() -> MockBody {
    MockBody::Events(vec![StreamEvent::Done {
        reason: StopReason::Stop,
    }])
}

/// Waits for the first event `pick` accepts, giving up rather than hanging.
async fn wait_for<T>(
    engine: &mut titi_engine::Engine,
    mut pick: impl FnMut(&EngineEvent) -> Option<T>,
) -> T {
    let deadline = Duration::from_secs(10);
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

/// A loop is a background turn the engine runs: the prompt reaches the model
/// without the surface sending anything after the first command.
#[tokio::test]
async fn a_loop_submits_its_prompt_until_it_is_cancelled() {
    let transport = Arc::new(MockTransport::new(vec![done(), done(), done()]));
    let captured = Arc::clone(&transport);
    let mut engine =
        EngineRuntime::start(EngineConfig::new("primary"), resolver("primary", transport));

    engine
        .send(EngineCommand::StartLoop {
            interval_secs: 1,
            prompt: "check the CI run".into(),
        })
        .await
        .unwrap();
    let job_id = wait_for(&mut engine, |event| match event {
        EngineEvent::JobStarted { job } => Some(job.id.clone()),
        _ => None,
    })
    .await;
    assert_eq!(job_id.as_str(), "job-1");

    // The turn the timer queued, not one the test submitted.
    wait_for(&mut engine, |event| match event {
        EngineEvent::TurnFinished { .. } => Some(()),
        _ => None,
    })
    .await;
    let sent = captured.requests();
    assert!(
        sent.iter().any(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content.contains("check the CI run"))
        }),
        "the loop prompt never reached the model: {sent:?}"
    );

    engine.send(EngineCommand::ListJobs).await.unwrap();
    let jobs = wait_for(&mut engine, |event| match event {
        EngineEvent::JobList { jobs } => Some(jobs.clone()),
        _ => None,
    })
    .await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].interval_secs, 1);
    assert!(jobs[0].runs >= 1, "{jobs:?}");

    engine
        .send(EngineCommand::CancelJob {
            job_id: job_id.clone(),
        })
        .await
        .unwrap();
    let stopped = wait_for(&mut engine, |event| match event {
        EngineEvent::JobFinished { job_id } => Some(job_id.clone()),
        _ => None,
    })
    .await;
    assert_eq!(stopped, job_id);

    engine.send(EngineCommand::ListJobs).await.unwrap();
    let jobs = wait_for(&mut engine, |event| match event {
        EngineEvent::JobList { jobs } => Some(jobs.clone()),
        _ => None,
    })
    .await;
    assert!(jobs.is_empty(), "{jobs:?}");
}

/// A loop nobody can read is refused, not silently normalised: a zero
/// interval would spin the engine and an empty prompt would ask nothing.
#[tokio::test]
async fn a_loop_without_an_interval_or_a_prompt_is_refused() {
    let transport = Arc::new(MockTransport::new(vec![done()]));
    let mut engine =
        EngineRuntime::start(EngineConfig::new("primary"), resolver("primary", transport));

    engine
        .send(EngineCommand::StartLoop {
            interval_secs: 0,
            prompt: "tick".into(),
        })
        .await
        .unwrap();
    let message = wait_for(&mut engine, |event| match event {
        EngineEvent::Failed { message, .. } => Some(message.to_string()),
        _ => None,
    })
    .await;
    assert!(message.contains("at least one second"), "{message}");

    engine
        .send(EngineCommand::StartLoop {
            interval_secs: 60,
            prompt: "   ".into(),
        })
        .await
        .unwrap();
    let message = wait_for(&mut engine, |event| match event {
        EngineEvent::Failed { message, .. } => Some(message.to_string()),
        _ => None,
    })
    .await;
    assert!(message.contains("needs a prompt"), "{message}");

    engine.send(EngineCommand::ListJobs).await.unwrap();
    let jobs = wait_for(&mut engine, |event| match event {
        EngineEvent::JobList { jobs } => Some(jobs.clone()),
        _ => None,
    })
    .await;
    assert!(jobs.is_empty(), "{jobs:?}");
}

/// Cancelling a job that is not there is an error, not a quiet success.
#[tokio::test]
async fn cancelling_an_unknown_job_reports_it() {
    let transport = Arc::new(MockTransport::new(vec![done()]));
    let mut engine =
        EngineRuntime::start(EngineConfig::new("primary"), resolver("primary", transport));

    engine
        .send(EngineCommand::CancelJob {
            job_id: "job-9".into(),
        })
        .await
        .unwrap();
    let message = wait_for(&mut engine, |event| match event {
        EngineEvent::Failed { message, .. } => Some(message.to_string()),
        _ => None,
    })
    .await;
    assert!(message.contains("no such job: job-9"), "{message}");
}
