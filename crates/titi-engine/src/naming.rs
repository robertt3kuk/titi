//! Best-effort session naming: a cheap model turns the first exchange into a
//! short title, so the session list stops showing opaque ids.
//!
//! Nothing here is on the critical path of a turn. The attempt runs in its
//! own task after the turn is already finished, and every way it can fail —
//! no role map, no key, no model, a slow or malformed answer — ends with the
//! session keeping the name it had. A failure is never reported to the turn,
//! because a title the user did not ask for is not worth an error.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures::StreamExt;
use smol_str::SmolStr;
use titi_core::session::SessionIndex;
use titi_core::session::namer::{clean_title, naming_prompt};
use titi_providers::{ChatMessage, RequestCtx, Role, StreamEvent, WireRequest};
use tokio::sync::mpsc;

use crate::protocol::EngineEvent;
use crate::runtime::TransportResolver;

/// Model role the namer runs on. Naming is not worth the primary model, and
/// the role map is where the user says which model is the cheap one.
const NAMING_ROLE: &str = "smol";
/// A title nobody is waiting for does not get to hold a task open forever.
const NAMING_TIMEOUT: Duration = Duration::from_secs(30);
/// Generation cap: three words need no more, and a chatty model is cut off
/// rather than paid for.
const NAMING_MAX_TOKENS: u32 = 64;

/// Runs blocking SQLite and config work off the async executor, discarding
/// every failure: this whole path is optional.
async fn off_thread<T: Send + 'static>(
    work: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    tokio::task::spawn_blocking(work).await.ok().flatten()
}

/// Names one session, once.
#[derive(Clone)]
pub struct SessionNamer {
    resolver: Arc<dyn TransportResolver>,
    events: mpsc::Sender<EngineEvent>,
    /// Holds `state.db`, where the session catalog lives.
    agent_dir: PathBuf,
    session_id: SmolStr,
    /// Project the config layers are read for.
    workspace: PathBuf,
    /// Model the turn ran on: what the `smol` role falls back to when the
    /// user configured no role map at all.
    current_model: SmolStr,
}

impl SessionNamer {
    pub fn new(
        resolver: Arc<dyn TransportResolver>,
        events: mpsc::Sender<EngineEvent>,
        agent_dir: PathBuf,
        session_id: impl Into<SmolStr>,
        workspace: PathBuf,
        current_model: impl Into<SmolStr>,
    ) -> Self {
        Self {
            resolver,
            events,
            agent_dir,
            session_id: session_id.into(),
            workspace,
            current_model: current_model.into(),
        }
    }

    /// Starts the attempt and returns at once. The caller — a finished turn —
    /// never waits for the model, the database, or the timeout.
    pub fn spawn(self, first_message: SmolStr) {
        tokio::spawn(async move {
            self.run(first_message).await;
        });
    }

    async fn run(self, first_message: SmolStr) -> Option<()> {
        if !self.needs_title().await? {
            return None;
        }
        let model = self.naming_model().await?;
        let answer = tokio::time::timeout(NAMING_TIMEOUT, self.ask(&model, &first_message))
            .await
            .ok()??;
        let title = clean_title(&answer)?;
        if !self.store(title.clone()).await? {
            return None;
        }
        let _ = self
            .events
            .send(EngineEvent::SessionNamed {
                session_id: self.session_id.clone(),
                title: title.into(),
            })
            .await;
        Some(())
    }

    /// Whether this session is unnamed — asked before a model is called, so a
    /// resumed session does not pay for a title it already has.
    async fn needs_title(&self) -> Option<bool> {
        let path = self.index_path();
        let session = self.session_id.to_string();
        off_thread(move || {
            SessionIndex::open(&path)
                .ok()?
                .needs_auto_title(&session)
                .ok()
        })
        .await
    }

    /// The model behind the `smol` role. An absent role map keeps the turn's
    /// own model, which is what the role resolver means by "current"; a map
    /// that does not name the role means no naming at all.
    async fn naming_model(&self) -> Option<SmolStr> {
        let agent_dir = self.agent_dir.clone();
        let workspace = self.workspace.clone();
        let current = self.current_model.to_string();
        off_thread(move || {
            let settings =
                titi_config::settings::Settings::load(&agent_dir, &workspace, &[]).ok()?;
            titi_config::roles::resolve_model_role(&settings, NAMING_ROLE, &current)
                .ok()
                .map(SmolStr::from)
        })
        .await
    }

    async fn ask(&self, model: &str, first_message: &str) -> Option<String> {
        let resolved = self.resolver.resolve(model).ok()?;
        let mut wire = WireRequest::new(resolved.wire_model.clone());
        wire.max_tokens = Some(NAMING_MAX_TOKENS);
        wire.messages.push(ChatMessage {
            role: Role::User,
            content: naming_prompt(first_message).into(),
            tool_calls: Vec::new(),
        });
        let ctx = RequestCtx {
            api_key: resolved.credential.map(|credential| credential.access),
            aborted: Arc::new(AtomicBool::new(false)),
        };
        let mut stream = resolved.transport.stream(wire, ctx).await.ok()?;
        let mut answer = String::new();
        while let Some(event) = stream.next().await {
            match event {
                // Thinking is deliberately left out: it is not the answer,
                // and pasted into a title it is noise.
                StreamEvent::TextDelta { text, .. } => answer.push_str(&text),
                StreamEvent::Error { .. } => return None,
                StreamEvent::Done { .. } => break,
                _ => {}
            }
        }
        Some(answer)
    }

    /// Writes the title, reporting whether it was taken. A `/rename` that
    /// landed while the model was talking wins: the store refuses to replace
    /// a name the user chose.
    async fn store(&self, title: String) -> Option<bool> {
        let path = self.index_path();
        let session = self.session_id.to_string();
        off_thread(move || {
            SessionIndex::open(&path)
                .ok()?
                .set_auto_title(&session, &title)
                .ok()
        })
        .await
    }

    fn index_path(&self) -> PathBuf {
        self.agent_dir.join("state.db")
    }
}

/// The message that names the session: the first thing the user asked in it.
pub fn first_user_message(messages: &[ChatMessage]) -> Option<SmolStr> {
    messages
        .iter()
        .find(|message| message.role == Role::User && !message.content.trim().is_empty())
        .map(|message| message.content.clone())
}
