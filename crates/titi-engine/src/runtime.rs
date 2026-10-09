use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures::StreamExt;
use smol_str::SmolStr;
use titi_genome::GenomeHandle;
use titi_genome::live::Request as IndexRequest;
use titi_providers::{
    ChatMessage, Credential, ErrorReason, RequestCtx, Role, StreamEvent, TokenUsage, Transport,
    TransportError, WireRequest,
};
use titi_tools::{ApprovalMode, ApprovalTier, ToolRegistry};
use tokio::sync::{mpsc, oneshot};

use crate::claims::Claims;
use crate::findings::Findings;
use crate::protocol::{ContextPart, EngineCommand, EngineEvent, TurnId};
use crate::registry::{ModelPrice, RefreshOutcome, RegistryError, ResolvedModel};
use crate::steering::Steering;
use crate::tool_loop::{
    ApprovalWaiters, ToolCallCollector, TouchedSink, TrajectorySink, execute_tools,
};

/// Identity the main turn claims files under.
const MAIN_AGENT: SmolStr = SmolStr::new_inline("Main");

/// Runs blocking map-building work off the async executor.
///
/// The map is an optimisation, never a precondition: a refresh that errors or
/// panics degrades to "no map this turn" instead of failing the turn. A panic
/// inside `spawn_blocking` surfaces as a `JoinError`, which this discards the
/// same way an `Err` is discarded.
async fn run_off_thread(
    work: impl FnOnce() -> Option<SmolStr> + Send + 'static,
) -> Option<SmolStr> {
    tokio::task::spawn_blocking(work).await.ok().flatten()
}

/// Model role a `/advisor` consult runs on.
const ADVISOR_ROLE: &str = "advisor";

/// The model behind the `advisor` role, when the settings name one.
///
/// A second opinion from the turn's own model is still a second opinion, so
/// an absent or unreadable role map is not a failure: the caller keeps the
/// model it already has.
async fn advisor_model(
    agent_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
    current: SmolStr,
) -> Option<SmolStr> {
    let agent_dir = agent_dir?;
    let workspace = workspace.unwrap_or_else(|| PathBuf::from("."));
    let current = current.to_string();
    run_off_thread(move || {
        let settings = titi_config::settings::Settings::load(&agent_dir, &workspace, &[]).ok()?;
        titi_config::roles::resolve_model_role(&settings, ADVISOR_ROLE, &current)
            .ok()
            .map(SmolStr::from)
    })
    .await
}

/// The standing instruction a mode adds to the system prompt, if any.
///
/// The tools already enforce the mode; this only tells the model what the
/// missing tools mean, so it answers with a plan instead of apologising for
/// an edit tool it cannot find.
fn mode_brief(mode: crate::protocol::SessionMode) -> Option<&'static str> {
    match mode {
        crate::protocol::SessionMode::Agent => None,
        crate::protocol::SessionMode::Plan => Some(
            "You are in plan mode. You can read the repository but you cannot \
change it: no write, edit, or shell tool is available to you this turn, and \
that is deliberate. Investigate, then answer with a plan — the files to \
change, what changes in each, and what could go wrong. Do not ask for the \
missing tools and do not pretend to have made the change.",
        ),
        crate::protocol::SessionMode::Duck => Some(
            "You are in duck mode: a thinking partner, not an agent. You cannot \
see this repository — no file, search, or shell tool is available to you, and \
no repository map is in this request. Do not guess at file contents or claim \
to have looked. Ask the user for anything you need to know about their code, \
and reason out loud with them.",
        ),
    }
}

/// Tiers a plan turn keeps.
///
/// Reads, and nothing else. [`ApprovalTier::Network`] is deliberately absent
/// rather than merely unlisted: a mode narrows what a turn may do, it never
/// widens it. Plan mode keeps the read tools, so a plan turn that could also
/// reach an outside host would be the one turn in the engine that can read
/// this repository and post it somewhere — and to do that without hanging in
/// headless it would have to auto-approve the call, which the agent turn it
/// precedes does not. A mode meant to be the careful one must not be the
/// loose one. Research belongs to the agent turn, where the user is asked.
const PLAN_TIERS: &[ApprovalTier] = &[ApprovalTier::Read];

/// Tiers a duck turn keeps.
///
/// The network tier and nothing else, which is the mode's promise stated as
/// a tier: a partner that cannot touch this machine. A read tool is harmless
/// but still shows it the repository, so reads stay out; the network tier is
/// the only one whose reach stops outside. Chosen by tier rather than by
/// tool name, so the mode does not depend on a list written when
/// `web_search` happened to be the only network tool, and a duck turn with
/// no network tool registered still goes out with no tools at all.
const DUCK_TIERS: &[ApprovalTier] = &[ApprovalTier::Network];

/// The memories relevant to this turn, ranked against the files it has
/// already touched. Off the runtime thread: the index is synchronous
/// SQLite, and blocking the runtime thread panics.
async fn recalled_memory(agent_dir: Option<PathBuf>, touched: &TouchedSink) -> Option<SmolStr> {
    let agent_dir = agent_dir?;
    let touched = Arc::clone(touched);
    run_off_thread(move || {
        let index = titi_memory::index::MemoryIndex::open(&agent_dir).ok()?;
        let touched = touched.blocking_lock().snapshot();
        let recalled = index.recall("", &touched).ok()?;
        let block = titi_memory::index::render_recall(&recalled);
        if block.is_empty() {
            None
        } else {
            Some(block.into())
        }
    })
    .await
}

/// How long a turn waits for the index before it names the backlog instead.
const GENOME_QUIESCE: std::time::Duration = std::time::Duration::from_millis(40);

/// Point the index at this turn's files and render the map from what it
/// publishes.
///
/// The turn no longer walks the tree itself. It hands the indexer the files
/// this session touched as the parse priority and asks for a resync of the
/// paths no tool named, then waits at most `quiesce` for that window to land.
/// When it lands, the map is what the old synchronous walk produced. When it
/// does not, the map is the **previous consistent graph** with its header
/// naming how much is outstanding — `<genome pending="3">` — so the model
/// reading it knows the map may be one batch behind and can re-read a file
/// instead of trusting it. A silent stale map is the one outcome the contract
/// forbids, and blocking until the walk finished is the latency this phase
/// exists to remove; the header is the honest middle.
///
/// `quiesce` is a parameter so a test can drive the not-caught-up branch
/// exactly rather than by racing a real walk; the turn passes
/// [`GENOME_QUIESCE`].
async fn genome_map(
    root: Option<PathBuf>,
    limit: usize,
    genome: Option<&GenomeHandle>,
    touched: &TouchedSink,
    quiesce: std::time::Duration,
) -> Option<SmolStr> {
    // The root is the guard here, not a path this function reads — the
    // handle was built from it. A turn whose config has none renders no map:
    // duck mode clears the root on its own clone and must stay repo-blind. A
    // root whose index could not be built has no handle either, which is the
    // same answer for the same reason.
    let genome = genome.filter(|_| root.is_some())?.clone();
    let touched = Arc::clone(touched);
    run_off_thread(move || {
        let touched: Vec<String> = touched.blocking_lock().snapshot();
        let mut urgent = touched.clone();
        urgent.reverse();
        genome.request(IndexRequest::Priority(urgent));
        genome.request(IndexRequest::Resync);
        // Wait, but do not decide on the wait's own answer. A wait that timed
        // out a microsecond before the batch landed would report a backlog
        // that is gone, and a batch that *failed* to apply reports nothing at
        // all through it. The count is the contract; the wait is the courtesy
        // that keeps the common case — and so the header — at zero.
        genome.quiesce(quiesce);
        // Read the backlog before the graph: a count that is still moving
        // means the batch had not landed when the snapshot was taken, and
        // over-reporting staleness costs a re-read, never a wrong answer.
        let pending = genome.pending();
        let (snapshot, _generation) = genome.snapshot();
        Some(SmolStr::from(
            snapshot.project_with_pending(limit, &touched, pending),
        ))
    })
    .await
}

/// The working-tree diff, off the async threads. Not a repo, no git, or a
/// call that does not finish in time is no snapshot — the turn still runs.
async fn working_tree_diff(
    root: Option<PathBuf>,
    policy: titi_tools::SensitivePolicy,
) -> Option<crate::difftrack::DiffSnapshot> {
    let root = root?;
    tokio::task::spawn_blocking(move || crate::difftrack::capture(&root, &policy))
        .await
        .ok()
        .flatten()
}

/// The turn's moving context, pinned to the message it was built for.
///
/// These blocks are rebuilt from scratch every turn. In the system prompt
/// they sat in front of the entire conversation, so one re-ranked file cost
/// the provider's cache every token behind them. Here they only ever
/// precede the prompt they belong to, and they stay attached to it when it
/// ages into history: everything the previous turn sent stays byte for byte
/// where it was, which is the only thing a prefix cache asks for.
///
/// `diff` is the working-tree snapshot for this turn. Empty or absent, the
/// bytes are what they were before the snapshot existed.
fn prompt_with_context(
    prompt: &str,
    recalled: Option<&str>,
    genome: Option<&str>,
    diff: Option<&str>,
) -> SmolStr {
    let diff = diff.and_then(render_diff);
    let mut parts: Vec<&str> = Vec::with_capacity(4);
    if let Some(block) = recalled.filter(|block| !block.is_empty()) {
        parts.push(block);
    }
    if let Some(block) = genome.filter(|block| !block.is_empty()) {
        parts.push(block);
    }
    if let Some(block) = diff.as_deref() {
        parts.push(block);
    }
    if parts.is_empty() {
        return prompt.into();
    }
    parts.push(prompt);
    parts.join("\n\n").into()
}

/// `<diff>` after the genome block. Whitespace-only text is not a snapshot.
fn render_diff(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    Some(format!("<diff>\n{}\n</diff>", text.trim_matches('\n')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_off_thread_failure_degrades_to_none() {
        assert_eq!(run_off_thread(|| None).await, None);
    }

    #[tokio::test]
    async fn an_off_thread_panic_degrades_to_none() {
        // The turn must survive a panicking index build.
        let outcome = run_off_thread(|| panic!("index build exploded")).await;
        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn a_successful_run_passes_the_map_through() {
        let map = run_off_thread(|| Some(SmolStr::new_inline("<genome>\n</genome>"))).await;
        assert_eq!(map.as_deref(), Some("<genome>\n</genome>"));
    }

    /// A turn whose index cannot catch up **names the backlog** instead of
    /// rendering a graph it knows may be behind.
    ///
    /// The deadline is a parameter so this drives the branch exactly: the turn
    /// passes [`GENOME_QUIESCE`] (40 ms), the test passes 1 ms and gives the
    /// index a tree whose first walk is far slower than that, so the outcome
    /// does not depend on racing a walk that happens to be quick. What is
    /// under test is the outcome — the model must see that the map it is
    /// reading is behind.
    #[tokio::test]
    async fn a_turn_that_cannot_wait_names_the_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The handle is spawned over an empty tree and the tree arrives after,
        // so the first walk is the slow one and the spawn itself is not.
        let genome = GenomeHandle::spawn(root, titi_genome::live::Options::default()).unwrap();
        for index in 0..150 {
            let mut body = String::new();
            for line in 0..120 {
                body.push_str(&format!("    let value_{line} = {line}_u64;\n"));
            }
            std::fs::write(
                root.join(format!("src_{index}.rs")),
                format!("pub fn f_{index}() {{\n{body}}}\n"),
            )
            .unwrap();
        }
        let touched = TouchedSink::default();

        let behind = genome_map(
            Some(root.to_path_buf()),
            8,
            Some(&genome),
            &touched,
            std::time::Duration::from_millis(1),
        )
        .await
        .expect("a map");
        assert!(
            behind.contains("<genome pending=\""),
            "a map that could not be brought current must say so: {behind}"
        );

        // Given time, the same call renders the graph the batch landed in.
        let settled = genome_map(
            Some(root.to_path_buf()),
            8,
            Some(&genome),
            &touched,
            std::time::Duration::from_secs(30),
        )
        .await
        .expect("a map");
        assert!(!settled.contains("pending="), "{settled}");
        assert!(settled.contains("src_0.rs"), "{settled}");
    }

    /// No root, no map — the guard the duck mode relies on.
    #[tokio::test]
    async fn a_turn_without_a_root_renders_no_map() {
        let touched = TouchedSink::default();
        assert!(
            genome_map(None, 8, None, &touched, GENOME_QUIESCE)
                .await
                .is_none()
        );
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "pub fn a() {}\n").unwrap();
        let genome = GenomeHandle::spawn(root, titi_genome::live::Options::default()).unwrap();
        assert!(
            genome_map(None, 8, Some(&genome), &touched, GENOME_QUIESCE)
                .await
                .is_none()
        );
    }

    #[test]
    fn the_volatile_blocks_sit_in_front_of_the_prompt() {
        let built =
            prompt_with_context("fix the parser", Some("<recall/>"), Some("<genome/>"), None);
        assert_eq!(built, "<recall/>\n\n<genome/>\n\nfix the parser");
        assert_eq!(prompt_with_context("bare", None, None, None), "bare");
        assert_eq!(prompt_with_context("bare", Some(""), None, None), "bare");
    }

    #[test]
    fn a_diff_block_follows_the_genome_and_is_absent_without_a_snapshot() {
        let added = "+fn added_for_the_turn() {}";
        let with = prompt_with_context(
            "fix the parser",
            Some("<recall/>"),
            Some("<genome/>"),
            Some(added),
        );
        assert!(with.contains("<diff>"), "{with}");
        assert!(with.contains(added), "{with}");
        assert!(with.contains("</diff>"), "{with}");
        assert_eq!(
            with.as_str(),
            [
                "<recall/>",
                "<genome/>",
                "<diff>\n+fn added_for_the_turn() {}\n</diff>",
                "fix the parser",
            ]
            .join("\n\n")
        );
        let genome_at = with.find("<genome/>").expect("genome");
        let diff_at = with.find("<diff>").expect("diff");
        let prompt_at = with.find("fix the parser").expect("prompt");
        assert!(genome_at < diff_at && diff_at < prompt_at, "{with}");

        let without =
            prompt_with_context("fix the parser", Some("<recall/>"), Some("<genome/>"), None);
        assert_eq!(without, "<recall/>\n\n<genome/>\n\nfix the parser");
        assert!(!without.contains("<diff>"), "{without}");
        assert_eq!(
            prompt_with_context(
                "fix the parser",
                Some("<recall/>"),
                Some("<genome/>"),
                Some(""),
            ),
            without
        );
        assert_eq!(
            prompt_with_context(
                "fix the parser",
                Some("<recall/>"),
                Some("<genome/>"),
                Some(" \n"),
            ),
            without
        );
        assert_eq!(prompt_with_context("bare", None, None, None), "bare");
    }

    /// The reason the blocks moved out of the system prompt: what one turn
    /// sent has to still be there, unchanged, when the next turn sends it
    /// again. The volatile text stays welded to the message it was built
    /// for instead of being rebuilt in front of the whole conversation.
    #[test]
    fn a_turn_leaves_the_previous_turns_messages_untouched() {
        let system = SmolStr::new("identity, project rules, skills");
        let first = prompt_with_context("first", Some("<recall v1/>"), Some("<genome v1/>"), None);
        let turn_one = vec![
            message(Role::System, &system),
            message(Role::User, &first),
            message(Role::Assistant, "an answer"),
        ];

        let history = visible_history(turn_one.clone(), Some(&system));
        let second =
            prompt_with_context("second", Some("<recall v2/>"), Some("<genome v2/>"), None);
        let mut turn_two = vec![message(Role::System, &system)];
        turn_two.extend(history);
        turn_two.push(message(Role::User, &second));

        assert_eq!(turn_two[..turn_one.len()], turn_one[..]);
        assert_eq!(turn_two.len(), turn_one.len() + 1);
        assert!(turn_two[1].content.contains("<genome v1/>"));
        assert!(turn_one[1].content.contains("<genome v1/>"));
    }

    fn message(role: Role, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: text.into(),
            tool_calls: Vec::new(),
            ..Default::default()
        }
    }

    fn folding_config() -> EngineConfig {
        let mut config = EngineConfig::new("primary");
        // Far below the 80% threshold: the automatic fold would not fire,
        // which is the whole reason `/compact` exists.
        config.context_window = 100_000;
        config.compaction.keep_recent_tokens = 4;
        config
    }

    #[test]
    fn manual_compaction_folds_below_the_threshold() {
        let mut config = folding_config();
        config.restored_messages = vec![
            message(Role::User, "the first question, long enough to fold"),
            message(Role::Assistant, "the first answer"),
            message(Role::User, "now"),
        ];

        let event = compact_history(&mut config, None, false, TurnId(7));

        match event {
            EngineEvent::Compacted {
                turn_id, folded, ..
            } => {
                assert_eq!(turn_id, TurnId(7));
                assert_eq!(folded, 2);
            }
            other => panic!("expected a compaction, got {other:?}"),
        }
        assert_eq!(config.restored_messages.len(), 2);
        assert_eq!(config.restored_messages[0].role, Role::System);
        assert_eq!(config.restored_messages[1].content, "now");
    }

    /// The digest keeps only the first line of each folded prompt, so a
    /// focus has to pull the rest of its thread back in or naming it does
    /// nothing.
    #[test]
    fn a_focused_compaction_keeps_the_lines_it_names() {
        let mut config = folding_config();
        config.restored_messages = vec![
            message(
                Role::User,
                "please review the diff\nthe auth guard moved to the middleware",
            ),
            message(
                Role::User,
                "another question\nabout pagination internals instead",
            ),
            message(Role::User, "now"),
        ];

        let event = compact_history(&mut config, Some("auth"), false, TurnId(1));

        assert!(matches!(event, EngineEvent::Compacted { .. }), "{event:?}");
        let digest = config.restored_messages[0].content.to_string();
        assert!(
            digest.contains("the auth guard moved to the middleware"),
            "{digest}"
        );
        assert!(!digest.contains("pagination internals"), "{digest}");
    }

    /// A running turn hands its own history back when it ends. Folding
    /// underneath it would be overwritten, or cost that turn its answer.
    #[test]
    fn manual_compaction_waits_for_a_running_turn() {
        let mut config = folding_config();
        config.restored_messages = vec![
            message(Role::User, "the first question, long enough to fold"),
            message(Role::Assistant, "the first answer"),
            message(Role::User, "now"),
        ];
        let before = config.restored_messages.clone();

        let event = compact_history(&mut config, None, true, TurnId(1));

        assert!(matches!(event, EngineEvent::Notice { .. }), "{event:?}");
        assert_eq!(config.restored_messages, before);
    }
}

/// Resolves a model id to its provider transport.
pub trait TransportResolver: Send + Sync + 'static {
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError>;

    /// What the named model costs, when the resolver knows.
    ///
    /// `None` is *unpriced*: a local model, a subscription backend, a model
    /// the registry discovered, or a price nobody wrote down. It is not a
    /// price of zero, and a caller that prints or enforces money must treat
    /// the two differently. The default is `None`, because a resolver that
    /// only maps ids to transports knows nothing about money.
    fn price(&self, _model: &str) -> Option<ModelPrice> {
        None
    }

    /// Renew the OAuth credentials a turn is about to resolve, in front of the
    /// first resolve. The default resolver owns no credential store.
    ///
    /// A boxed future because the runtime reaches its resolver through `dyn`.
    fn refresh_due(&self) -> Pin<Box<dyn Future<Output = Vec<RefreshOutcome>> + Send + '_>> {
        Box::pin(async { Vec::new() })
    }
}

impl<F> TransportResolver for F
where
    F: Fn(&str) -> Result<ResolvedModel, RegistryError> + Send + Sync + 'static,
{
    fn resolve(&self, model: &str) -> Result<ResolvedModel, RegistryError> {
        self(model)
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub primary_model: SmolStr,
    pub fallback_models: Vec<SmolStr>,
    pub max_transient_retries: u32,
    /// Wait before the first transient retry; each later one doubles it, up
    /// to [`MAX_RETRY_BACKOFF`]. A 429 answered at once is a retry spent.
    pub retry_backoff: std::time::Duration,
    pub command_capacity: usize,
    pub event_capacity: usize,
    pub max_tool_rounds: u32,
    pub approval_mode: ApprovalMode,
    /// Workspace root kept indexed turn-by-turn. `None` disables the Genome.
    pub genome_root: Option<PathBuf>,
    /// Ranked files injected per prompt.
    pub genome_limit: usize,
    /// Workspace a subagent's tools are jailed to. `None` keeps its registry
    /// empty even when `agent_model` is set.
    pub workspace_root: Option<PathBuf>,
    /// Model a spawned subagent runs on. `None` disables spawning: without a
    /// runner the supervisor is never built.
    pub agent_model: Option<SmolStr>,
    /// Whether a subagent may write and execute. Off by default: a subagent
    /// has no approval surface, so it gets read tools only.
    pub agent_writes: bool,
    /// Tool rounds a subagent may spend before it is stopped.
    pub agent_rounds: u32,
    /// Read cache shared with the subagent's read tool, so a file the main
    /// turn read is not read again from disk.
    pub read_cache: titi_tools::ReadCache,
    /// Conversation replayed from a persisted session, prepended to every
    /// prompt so a resumed session keeps its history.
    pub restored_messages: Vec<ChatMessage>,
    /// Context window the model reports, used to decide when to fold.
    pub context_window: u64,
    /// When and how the oldest messages are folded away.
    pub compaction: titi_core::compaction::CompactionPolicy,
    /// Agent directory holding `SOUL.md`, `PERSONALITY.md` and the memory
    /// index. `None` sends no identity — only the genome map.
    pub agent_dir: Option<PathBuf>,
    /// Embeddings model from `memory.embeddingModel`. `None` uses the local
    /// trigram embedder, which needs no network.
    pub embedding_model: Option<String>,
    /// Which files the subagent's tools refuse as credentials. The surface
    /// builds the main turn's tools with the same policy.
    pub sensitive: titi_tools::SensitivePolicy,
    /// Mask IPv4 addresses in tool output (`privacy.maskIps`). Keys are
    /// masked regardless.
    pub mask_ips: bool,
    /// Checks run against the coder's patch before a reviewer is called
    /// (`goal.gates`). Each entry is a program and its arguments; an empty
    /// list sends every patch straight to the reviewer.
    pub goal_gates: Vec<crate::goal::GateCommand>,
    /// Session the turns are recorded under, so a finished turn can name it.
    /// `None` (headless one-shots, tests) simply never names anything.
    pub session_id: Option<String>,
    pub judgment_provider: Option<crate::judgment::JudgmentProvider>,
    /// Mode the session starts in (`--mode plan|duck`). `SetMode` changes it
    /// afterwards.
    pub mode: crate::protocol::SessionMode,
    /// Stops the command a `bash` call is waiting on. The surface builds its
    /// workspace tools with a clone, as the runtime does a subagent's; a
    /// cancel raises it and the next turn lowers it as it starts.
    pub interrupt: titi_tools::Interrupt,
    /// How long a `bash` call may hold a turn before it is handed to the
    /// background, where its output reaches the session when it ends.
    ///
    /// The surface resolves the setting — `bash.autoBackground.thresholdMs`,
    /// with `TITI_BASH_BACKGROUND_MS` overriding it, through
    /// [`titi_tools::background_after_with`] — and passes the result here. A
    /// caller that passes `None` gets [`titi_tools::background_after`]: the
    /// environment variable, then [`titi_tools::BACKGROUND_AFTER`]. Tests set
    /// a tiny one so a `sleep 5` need not take five seconds.
    pub background_after: Option<std::time::Duration>,
}

impl EngineConfig {
    pub fn new(primary_model: impl Into<SmolStr>) -> Self {
        Self {
            primary_model: primary_model.into(),
            fallback_models: Vec::new(),
            max_transient_retries: 2,
            retry_backoff: std::time::Duration::from_millis(500),
            command_capacity: 64,
            event_capacity: 256,
            max_tool_rounds: 8,
            approval_mode: ApprovalMode::Write,
            genome_root: None,
            genome_limit: 24,
            workspace_root: None,
            agent_model: None,
            agent_writes: false,
            agent_rounds: crate::tool_agent::DEFAULT_AGENT_ROUNDS,
            read_cache: titi_tools::ReadCache::default(),
            restored_messages: Vec::new(),
            context_window: 128_000,
            compaction: titi_core::compaction::CompactionPolicy::default(),
            agent_dir: None,
            embedding_model: None,
            sensitive: titi_tools::SensitivePolicy::default(),
            mask_ips: true,
            goal_gates: Vec::new(),
            session_id: None,
            judgment_provider: None,
            mode: crate::protocol::SessionMode::Agent,
            interrupt: titi_tools::Interrupt::new(),
            background_after: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine command channel closed")]
    CommandChannelClosed,
}

/// Surface-side handle. TUI, GPUI, and headless RPC use the same contract.
pub struct Engine {
    commands: mpsc::Sender<EngineCommand>,
    events: mpsc::Receiver<EngineEvent>,
    claims: Claims,
    findings: Findings,
}

impl Engine {
    pub async fn send(&self, command: EngineCommand) -> Result<(), EngineError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| EngineError::CommandChannelClosed)
    }

    pub async fn recv(&mut self) -> Option<EngineEvent> {
        self.events.recv().await
    }

    pub fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        self.events.try_recv()
    }

    pub fn try_send(&self, command: EngineCommand) -> Result<(), EngineError> {
        self.commands
            .try_send(command)
            .map_err(|_| EngineError::CommandChannelClosed)
    }

    /// The runtime's write-claim table, so a surface can show who holds what
    /// (or hold a file itself before dispatching work).
    pub fn claims(&self) -> &Claims {
        &self.claims
    }

    /// Findings subagents have reported, in order.
    pub fn findings(&self) -> &Findings {
        &self.findings
    }
}

/// The background jobs a runtime is holding, by job id: the loops it repeats
/// on its own timer, and the handed-over `bash` commands it is waiting on.
type JobTable = std::collections::HashMap<SmolStr, Job>;

/// One background job. `/jobs` lists both kinds and `/jobs cancel` stops
/// either, which is why they share a table and a vocabulary.
enum Job {
    Loop(LoopJob),
    Command(CommandJob),
}

impl Job {
    /// Order the job was started in, so `/jobs` lists job-10 after job-9 and
    /// does not shuffle between reads.
    fn seq(&self) -> u64 {
        match self {
            Job::Loop(job) => job.seq,
            Job::Command(job) => job.seq,
        }
    }

    fn info(&self, id: &SmolStr) -> crate::protocol::JobInfo {
        match self {
            Job::Loop(job) => job.info(id),
            Job::Command(job) => job.info(id),
        }
    }
}

/// One `/loop` job: what it sends, how often, and the timer sending it.
struct LoopJob {
    /// Order the job was started in, so `/jobs` lists job-10 after job-9.
    seq: u64,
    prompt: SmolStr,
    interval_secs: u64,
    /// Bumped by the timer task after every prompt it queued, so `/jobs`
    /// reports what actually ran rather than what was scheduled.
    runs: Arc<AtomicU64>,
    timer: tokio::task::JoinHandle<()>,
}

impl LoopJob {
    fn info(&self, id: &SmolStr) -> crate::protocol::JobInfo {
        crate::protocol::JobInfo {
            id: id.clone(),
            prompt: self.prompt.clone(),
            interval_secs: self.interval_secs,
            runs: self.runs.load(Ordering::SeqCst),
        }
    }
}

/// One `bash` command a turn handed over after its background threshold: what
/// it was, and the handle that stops it. The thread waiting on the command
/// reports the end, so cancelling only has to signal it.
struct CommandJob {
    seq: u64,
    command: SmolStr,
    cancel: titi_tools::BackgroundCancel,
}

impl CommandJob {
    fn info(&self, id: &SmolStr) -> crate::protocol::JobInfo {
        crate::protocol::JobInfo {
            id: id.clone(),
            prompt: self.command.clone(),
            // A command runs once, on no timer of ours.
            interval_secs: 0,
            runs: 0,
        }
    }
}

/// The engine's end of `bash`'s background seam: it mints the job id, records
/// the command beside the loops so `/jobs` and `/jobs cancel` cover it, and
/// waits for it off the runtime's thread so its output reaches the session
/// when it ends.
struct BackgroundJobs {
    /// How long a `bash` call may hold a turn before it is handed over.
    after: std::time::Duration,
    /// Whether the session masks IPv4 addresses in what a tool prints. Keys
    /// are masked either way.
    mask_ips: bool,
    seq: Arc<AtomicU64>,
    jobs: Arc<Mutex<JobTable>>,
    events: mpsc::Sender<EngineEvent>,
    commands: mpsc::Sender<EngineCommand>,
}

impl titi_tools::BackgroundSink for BackgroundJobs {
    fn after(&self) -> std::time::Duration {
        self.after
    }

    fn hand_over(&self, command: &str, background: titi_tools::Background) -> SmolStr {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let id = SmolStr::from(format!("bg-{seq}"));
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.insert(
                id.clone(),
                Job::Command(CommandJob {
                    seq,
                    command: SmolStr::from(command),
                    cancel: background.cancel(),
                }),
            );
        }
        let events = self.events.clone();
        let commands = self.commands.clone();
        let jobs = Arc::clone(&self.jobs);
        let report_id = id.clone();
        let report_command = command.to_owned();
        let mask_ips = self.mask_ips;
        tokio::spawn(async move {
            // Off the async workers: this is the same wait the turn would
            // have done inline, only nobody is holding a turn for it.
            let run = match tokio::task::spawn_blocking(move || background.wait()).await {
                Ok(run) => run,
                Err(error) => titi_tools::pipe::Run {
                    output: format!("could not wait on the command: {error}"),
                    exit_code: None,
                    success: false,
                },
            };
            if let Ok(mut jobs) = jobs.lock() {
                jobs.remove(&report_id);
            }
            let _ = events
                .send(EngineEvent::JobFinished {
                    job_id: report_id.clone(),
                })
                .await;
            // The output reaches the session the way a loop's prompt does:
            // a follow-up prompt, which queues behind a live turn instead of
            // interrupting it.
            let _ = commands
                .send(EngineCommand::FollowUp {
                    text: job_report(&report_id, &report_command, &run, mask_ips).into(),
                })
                .await;
        });
        id
    }
}

/// What a finished background command reports into the session: which job it
/// was, how it exited, what it ran, and what it printed. It goes through the
/// same mask and the same [`crate::tool_loop::cap_output`] a tool result does,
/// so a backgrounded command cannot leak more, or spend more, than a
/// foreground one.
fn job_report(id: &str, command: &str, run: &titi_tools::pipe::Run, mask_ips: bool) -> String {
    let status = run.exit_code.map_or_else(
        || "killed by a signal".to_owned(),
        |code| format!("exit {code}"),
    );
    let report = format!(
        "background job {id} finished, {status}\n$ {command}\n{}",
        run.output
    );
    crate::tool_loop::cap_output(&crate::tool_loop::mask(&report, mask_ips))
}

/// The questions the model has asked and nobody has answered yet, by request
/// id. The turn is parked on the receiving end of one of these; the command
/// loop holds the sending end.
pub(crate) type AskWaiters =
    Arc<tokio::sync::Mutex<HashMap<SmolStr, oneshot::Sender<titi_tools::AskAnswer>>>>;

/// The session's door for the `ask` tool: one question out as an event, one
/// answer back as a command, and the tool parked in between.
///
/// Session-wide rather than per turn, because the tool that holds it is built
/// with the registry and reached through `&self`. What a turn contributes is
/// the interrupt: a cancel raises the session's [`titi_tools::Interrupt`], and
/// this is what turns that into the tool's `Cancelled` answer instead of a wait
/// that outlives the turn that started it.
struct SessionAsk {
    events: mpsc::Sender<EngineEvent>,
    waiters: AskWaiters,
    interrupt: titi_tools::Interrupt,
    /// Numbers the requests. A surface needs an id to answer with, and it has
    /// to be unique across the session, not merely across a turn.
    next: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl titi_tools::AskSink for SessionAsk {
    async fn ask(&self, request: titi_tools::AskRequest) -> titi_tools::AskAnswer {
        let request_id = SmolStr::from(format!(
            "ask-{}",
            self.next.fetch_add(1, Ordering::SeqCst) + 1
        ));
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(request_id.clone(), tx);
        // Marked before the event goes out: a cancel that lands while the
        // question is being delivered is still a cancel of this question.
        let mark = self.interrupt.mark();
        let _ = self
            .events
            .send(EngineEvent::AskRequested {
                request_id: request_id.clone(),
                question: request.question.clone().into(),
                options: request.options.iter().map(SmolStr::from).collect(),
                multi: request.multi,
                free_text: request.free_text,
            })
            .await;
        let answer = tokio::select! {
            answer = rx => answer.unwrap_or(titi_tools::AskAnswer::Cancelled),
            _ = wait_interrupted(&self.interrupt, mark) => titi_tools::AskAnswer::Cancelled,
        };
        // The wait is over either way, so the entry goes: an answer that
        // arrives afterwards has nothing left to unblock.
        self.waiters.lock().await.remove(&request_id);
        answer
    }
}

/// Waits until the session's interrupt is raised, or was raised since `mark`.
///
/// The same shape as the approval wait's abort poll: there is no channel to
/// select on for "the user cancelled", only the flag the cancel raises, so the
/// wait yields and looks again.
async fn wait_interrupted(interrupt: &titi_tools::Interrupt, mark: u64) {
    while !interrupt.raised_since(mark) {
        tokio::task::yield_now().await;
    }
}

/// UI-independent command loop and turn scheduler.
pub struct EngineRuntime {
    config: EngineConfig,
    resolver: Arc<dyn TransportResolver>,
    commands: mpsc::Receiver<EngineCommand>,
    events: mpsc::Sender<EngineEvent>,
    next_turn: Arc<AtomicU64>,
    agents: Option<crate::agents::AgentSupervisor>,
    tools: ToolRegistry,
    approval_waiters: ApprovalWaiters,
    /// The questions the model has asked and nobody has answered yet. See
    /// [`SessionAsk`].
    ask_waiters: AskWaiters,
    trajectory: TrajectorySink,
    /// The session's live index and the background worker behind it, when a
    /// root is configured. Written by the tool loop as it runs — synchronously,
    /// so a turn's own prompt is current by construction — and fed a resync per
    /// turn through the worker, whose backlog the map names when it cannot
    /// catch up inside [`GENOME_QUIESCE`].
    genome: Option<GenomeHandle>,
    /// Files this session read or edited; boosts their rank in the projection.
    touched: TouchedSink,
    /// Per-file write claims shared by every agent in this runtime.
    claims: Claims,
    /// Messages typed mid-flight, injected at the next step boundary.
    steering: Steering,
    /// Bumped by every `RestoreHistory`. A turn that started before the
    /// rewind must not write its stale history back over the replacement.
    history_epoch: u64,
    /// The background jobs the engine is holding: loops it repeats, and
    /// `bash` commands a turn handed over. Shared with the sink the tools
    /// report through, so `/jobs` sees a command the moment it is handed over.
    jobs: Arc<Mutex<JobTable>>,
    /// Numbers the next background job is named after, shared for the same
    /// reason. Loops take `job-N` and handed-over commands `bg-N`, so an id
    /// says which kind it is.
    next_job: Arc<AtomicU64>,
    /// A clone of the surface's command sender, so a background job can
    /// queue its prompt through the same door every other prompt uses.
    self_commands: mpsc::Sender<EngineCommand>,
    /// Tokens this session has spent, summed by the turns themselves.
    spent: Arc<AtomicU64>,
    /// The cap `/budget` set, if any.
    budget: Option<u64>,
    /// The cap has already been reported as reached, so the surface is not
    /// told again for every prompt that is refused afterwards.
    budget_tripped: bool,
    /// Micro-dollars this session's priced turns have cost, summed by the same
    /// turns that report their tokens — one figure, one place, so a footer and
    /// this ledger cannot disagree.
    spent_micro_usd: Arc<AtomicU64>,
    /// The money cap, in micro-dollars, if any. Independent of the token cap:
    /// a session may have either, both, or neither.
    money_budget: Option<u64>,
    /// As [`Self::budget_tripped`], for the money cap.
    money_tripped: bool,
    /// A turn has already run on a model this session cannot price while a
    /// money cap was in force, and the surface has been told. Set by the turn
    /// that found out — it is the one holding the model — and cleared when a
    /// new cap is set, because a new cap is a fresh start.
    money_unpriced_said: Arc<AtomicBool>,
    /// What the next turns are allowed to do.
    mode: crate::protocol::SessionMode,
}

impl EngineRuntime {
    pub fn start(config: EngineConfig, resolver: Arc<dyn TransportResolver>) -> Engine {
        Self::start_inner(
            config,
            resolver,
            None,
            ToolRegistry::new(),
            TrajectorySink::default(),
        )
    }

    pub fn start_with_agents(
        config: EngineConfig,
        resolver: Arc<dyn TransportResolver>,
        runner: Arc<dyn crate::agents::AgentRunner>,
    ) -> Engine {
        Self::start_inner(
            config,
            resolver,
            Some(runner),
            ToolRegistry::new(),
            TrajectorySink::default(),
        )
    }

    pub fn start_with_tools(
        config: EngineConfig,
        resolver: Arc<dyn TransportResolver>,
        tools: ToolRegistry,
    ) -> Engine {
        Self::start_inner(config, resolver, None, tools, TrajectorySink::default())
    }

    pub fn start_with_agents_and_tools(
        config: EngineConfig,
        resolver: Arc<dyn TransportResolver>,
        runner: Arc<dyn crate::agents::AgentRunner>,
        tools: ToolRegistry,
    ) -> Engine {
        Self::start_inner(
            config,
            resolver,
            Some(runner),
            tools,
            TrajectorySink::default(),
        )
    }

    pub fn start_with_session(
        config: EngineConfig,
        resolver: Arc<dyn TransportResolver>,
        runner: Option<Arc<dyn crate::agents::AgentRunner>>,
        tools: ToolRegistry,
        trajectory: TrajectorySink,
    ) -> Engine {
        Self::start_inner(config, resolver, runner, tools, trajectory)
    }

    fn start_inner(
        config: EngineConfig,
        resolver: Arc<dyn TransportResolver>,
        runner: Option<Arc<dyn crate::agents::AgentRunner>>,
        mut tools: ToolRegistry,
        trajectory: TrajectorySink,
    ) -> Engine {
        let (command_tx, command_rx) = mpsc::channel(config.command_capacity);
        let (event_tx, event_rx) = mpsc::channel(config.event_capacity);
        let claims = Claims::new();
        let findings = Findings::default();
        let touched: TouchedSink = TouchedSink::default();
        let jobs = Arc::new(Mutex::new(JobTable::new()));
        let next_job = Arc::new(AtomicU64::new(1));
        // A command a turn hands over lands in the table `/jobs` reads, and
        // reports back through the same follow-up door a loop's prompt uses.
        // The threshold is the session's, not the tool's: the environment
        // unless the config names one.
        let background: Arc<dyn titi_tools::BackgroundSink> = Arc::new(BackgroundJobs {
            after: config
                .background_after
                .unwrap_or_else(titi_tools::background_after),
            mask_ips: config.mask_ips,
            seq: Arc::clone(&next_job),
            jobs: Arc::clone(&jobs),
            events: event_tx.clone(),
            commands: command_tx.clone(),
        });
        // A subagent shares the runtime's claim table, touched-file set, read
        // cache and findings bus, so it cannot write a file the parent holds,
        // its reads warm the parent's cache, and the parent can read what it
        // learned.
        // The index is built once, here, and its worker started behind it: a
        // root that cannot be read leaves the session without a map, which is
        // the same degradation the per-turn refresh already had, and a cold
        // start is a full walk either way — doing it now rather than inside
        // the first turn is what lets the first turn's map be a snapshot
        // instead of a walk. It is built before the subagent runner below so
        // that runner can fold its own writes in the same way the main turn
        // does.
        let genome = config
            .genome_root
            .as_ref()
            .and_then(|root| GenomeHandle::spawn(root, titi_genome::live::Options::default()).ok());
        // The model's questions reach the surface through this door, and the
        // answers come back as commands. Installed here because it is the only
        // place both ends exist at once: the registry that holds the tool, and
        // the command loop that resolves the answer. The subagent's registry
        // below is built without it, which is the honest state for a runner
        // whose events go nowhere.
        let ask_waiters: AskWaiters = AskWaiters::default();
        tools.install_ask(Arc::new(SessionAsk {
            events: event_tx.clone(),
            waiters: Arc::clone(&ask_waiters),
            interrupt: config.interrupt.clone(),
            next: Arc::new(AtomicU64::new(0)),
        }));
        let runner = runner.or_else(|| {
            let model = config.agent_model.clone()?;
            let root = config.workspace_root.clone()?;
            let mut tools = ToolRegistry::new();
            for tool in titi_tools::workspace_tools_with_interrupt(
                &root,
                config.read_cache.clone(),
                config.sensitive.clone(),
                config.interrupt.clone(),
            ) {
                tools.register(Arc::from(tool));
            }
            // A subagent's `bash` hands over to the same table the parent's
            // does: there is one session, so there is one `/jobs`.
            tools.install_background(Arc::clone(&background));
            // The registry is the whole policy: a subagent has no surface to
            // show an approval on, so whatever it is handed it must be able
            // to run. Without writes, everything above read tier is dropped
            // and `Write` already auto-approves the rest. With writes, the
            // write and exec tools are there on purpose, and leaving the
            // runner on `Write` would park the first call on an approval
            // nobody can answer.
            let approval = if config.agent_writes {
                ApprovalMode::Yolo
            } else {
                tools.retain_tiers(&[ApprovalTier::Read]);
                ApprovalMode::Write
            };
            Some(Arc::new(
                crate::tool_agent::ToolAgentRunner::new(
                    Arc::clone(&resolver),
                    model,
                    tools,
                    claims.clone(),
                    Arc::clone(&touched),
                )
                .with_approval_mode(approval)
                .with_max_rounds(config.agent_rounds)
                .with_mask_ips(config.mask_ips)
                .with_genome(genome.clone()),
            ) as Arc<dyn crate::agents::AgentRunner>)
        });
        // The surface builds the session's tools before the engine starts —
        // a subagent's registry is built here — so this is where `bash`
        // learns where a command that outlives the turn goes.
        tools.install_background(Arc::clone(&background));
        let agents = runner.map(|runner| {
            crate::agents::AgentSupervisor::with_state(
                runner,
                event_tx.clone(),
                claims.clone(),
                findings.clone(),
            )
        });
        // The model's own door to the supervisor, and the same spawn the
        // surface's `SpawnAgent` command uses: one path, so a subagent the
        // model asked for is a subagent the surface can focus, revive and
        // stop like any other. A session with no supervisor gets no `agent`
        // tool rather than one that refuses every call. The subagent's own
        // registry is built below without this tool, which is what bounds
        // nesting: a subagent cannot spawn a subagent, at any depth.
        if let Some(agents) = &agents {
            tools.register(Arc::new(crate::agent_tool::AgentTool::new(
                agents.clone(),
                config.agent_writes,
            )));
        }
        let runtime = Self {
            mode: config.mode,
            config,
            resolver,
            commands: command_rx,
            events: event_tx,
            next_turn: Arc::new(AtomicU64::new(1)),
            agents,
            tools,
            approval_waiters: ApprovalWaiters::default(),
            ask_waiters,
            trajectory,
            genome,
            touched,
            claims: claims.clone(),
            steering: Steering::default(),
            history_epoch: 0,
            jobs,
            next_job,
            self_commands: command_tx.clone(),
            spent: Arc::new(AtomicU64::new(0)),
            budget: None,
            budget_tripped: false,
            spent_micro_usd: Arc::new(AtomicU64::new(0)),
            money_budget: None,
            money_tripped: false,
            money_unpriced_said: Arc::new(AtomicBool::new(false)),
        };
        tokio::spawn(runtime.run());
        Engine {
            commands: command_tx,
            events: event_rx,
            claims,
            findings,
        }
    }

    async fn run(mut self) {
        let (done_tx, mut done_rx) = mpsc::channel::<TurnDone>(8);
        let mut active: Option<(TurnId, Arc<AtomicBool>)> = None;
        let mut queued = VecDeque::<SmolStr>::new();
        let mut primary_model = self.config.primary_model.clone();

        // A session started with `--mode plan|duck` has a badge to fill in:
        // the surface learns the mode the same way it learns every later
        // change, so it never has to assume one.
        if self.mode != crate::protocol::SessionMode::Agent {
            let _ = self
                .events
                .send(EngineEvent::ModeChanged { mode: self.mode })
                .await;
        }

        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        EngineCommand::SubmitPrompt { text } | EngineCommand::FollowUp { text } => {
                            if self.over_budget().await {
                                // Nothing is queued: the prompt would only
                                // wait for a cap that nothing lifts on its own.
                                let _ = self.events.send(EngineEvent::PromptReturned { text }).await;
                            } else if active.is_some() {
                                queued.push_back(text);
                            } else {
                                let text = self.expand_skills(text).await;
                                let system = self.system_prompt().await;
                                active = Some(self.spawn_turn(text, primary_model.clone(), system, done_tx.clone()));
                            }
                        }
                        EngineCommand::RestoreHistory { messages } => {
                            self.config.restored_messages = messages;
                            self.history_epoch += 1;
                        }
                        EngineCommand::Steer { text } => {
                            // Queued, not applied here: the running turn drains it at
                            // its next step boundary, so steering never aborts work.
                            self.steering.push(text);
                        }
                        EngineCommand::Cancel => {
                            if let Some((turn_id, aborted)) = active.take() {
                                aborted.store(true, Ordering::SeqCst);
                                // The abort flag is only read between steps; a
                                // shell command the turn is waiting on has to be
                                // stopped, or the cancel waits for it to finish.
                                self.config.interrupt.raise();
                                let _ = self.events.send(EngineEvent::Cancelled { turn_id }).await;
                            }
                            // A subagent the turn spawned is the turn's work.
                            // The abort flag alone would not reach it: the
                            // tool call that spawned it is mid-await, and the
                            // loop only reads the flag between calls. Stop
                            // them here — the same stop `StopAgent` asks for —
                            // so a cancelled turn does not leave an agent
                            // running, and holding files, behind it.
                            if let Some(agents) = &self.agents {
                                agents.stop_all().await;
                            }
                            // Cancel is a stop, and taking `active` already stops
                            // the drain on the next `TurnDone`. Left in place the
                            // queue would sit there and fire at some later turn's
                            // end, answering a question the user walked away from.
                            // The text goes back to the surface instead.
                            for text in queued.drain(..) {
                                let _ = self.events.send(EngineEvent::PromptReturned { text }).await;
                            }
                            // Steering the cancelled turn never read goes back
                            // the same way, or it would ride along with the
                            // next prompt the user sends.
                            for text in self.steering.drain() {
                                let _ = self.events.send(EngineEvent::PromptReturned { text }).await;
                            }
                        }
                        EngineCommand::SwitchModel { model } => {
                            // Answer, even when the model does not change: a
                            // surface that waits for the outcome must not hang
                            // on a switch it cannot observe. The event carries
                            // no turn because a standalone switch has none.
                            let from = std::mem::replace(&mut primary_model, model.clone());
                            let _ = self.events.send(EngineEvent::ModelSwitched {
                                turn_id: None,
                                from,
                                to: model,
                            }).await;
                        }
                        EngineCommand::RunGoal { text } => {
                            self.spawn_goal(text, primary_model.clone());
                        }
                        EngineCommand::RunCouncil { question } => {
                            self.spawn_council(question, primary_model.clone());
                        }
                        EngineCommand::RunGraph { task } => {
                            self.spawn_graph(task, primary_model.clone());
                        }
                        EngineCommand::MemoryList => {
                            if let Some(agent_dir) = self.config.agent_dir.clone() {
                                let output = run_off_thread(move || {
                                    let index = titi_memory::index::MemoryIndex::open(&agent_dir).ok()?;
                                    let entries = index.recent(titi_memory::index::PAGE_MAX).unwrap_or_default();
                                    if entries.is_empty() {
                                        Some("no memories found".into())
                                    } else {
                                        let out = entries
                                            .into_iter()
                                            .map(|e| format!("{:>4} | {} | {} | {}", e.id, e.created_at, e.category, e.preview))
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        Some(out.into())
                                    }
                                })
                                .await
                                .unwrap_or_else(|| "memory index unavailable".into());
                                let _ = self.events.send(EngineEvent::MemoryResult { output }).await;
                            } else {
                                let _ = self.events.send(EngineEvent::MemoryResult { output: "no agent directory".into() }).await;
                            }
                        }
                        EngineCommand::MemorySearch { query } => {
                            let q = query.to_string();
                            if let Some(agent_dir) = self.config.agent_dir.clone() {
                                let output = run_off_thread(move || {
                                    let index = titi_memory::index::MemoryIndex::open(&agent_dir).ok()?;
                                    let entries = index.search(&q, titi_memory::index::PAGE_MAX).unwrap_or_default();
                                    if entries.is_empty() {
                                        Some("no memories found".into())
                                    } else {
                                        let out = entries
                                            .into_iter()
                                            .map(|e| format!("{:>4} | {} | {} | {}", e.id, e.created_at, e.category, e.preview))
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        Some(out.into())
                                    }
                                })
                                .await
                                .unwrap_or_else(|| "memory index unavailable".into());
                                let _ = self.events.send(EngineEvent::MemoryResult { output }).await;
                            } else {
                                let _ = self.events.send(EngineEvent::MemoryResult { output: "no agent directory".into() }).await;
                            }
                        }
                        EngineCommand::MemoryForget { id } => {
                            if let Some(agent_dir) = self.config.agent_dir.clone() {
                                let output = run_off_thread(move || {
                                    let index = titi_memory::index::MemoryIndex::open(&agent_dir).ok()?;
                                    if index.forget(id).is_ok() {
                                        Some(format!("forgot memory {id}").into())
                                    } else {
                                        Some(format!("failed to forget memory {id}").into())
                                    }
                                })
                                .await
                                .unwrap_or_else(|| "memory index unavailable".into());
                                let _ = self.events.send(EngineEvent::MemoryResult { output }).await;
                            } else {
                                let _ = self.events.send(EngineEvent::MemoryResult { output: "no agent directory".into() }).await;
                            }
                        }
                        EngineCommand::StartLoop { interval_secs, prompt } => {
                            self.start_loop(interval_secs, prompt).await;
                        }
                        EngineCommand::ListJobs => {
                            let jobs = self.job_list();
                            let _ = self.events.send(EngineEvent::JobList { jobs }).await;
                        }
                        EngineCommand::CancelJob { job_id } => {
                            let job = match self.jobs.lock() {
                                Ok(mut jobs) => jobs.remove(&job_id),
                                Err(_) => None,
                            };
                            match job {
                                Some(Job::Loop(job)) => {
                                    job.timer.abort();
                                    let _ = self.events.send(EngineEvent::JobFinished { job_id }).await;
                                }
                                // Only signal it: the thread waiting on the
                                // command reports the end once its group is
                                // really down, so nobody is told a job
                                // stopped while it still runs.
                                Some(Job::Command(job)) => job.cancel.cancel(),
                                None => self.emit_control_failure(&format!("no such job: {job_id}")).await,
                            }
                        }
                        EngineCommand::Consult { question } => {
                            self.spawn_consult(question, primary_model.clone());
                        }
                        EngineCommand::AnswerAsk { request_id, answer } => {
                            // The wait is gone when the turn it belonged to is.
                            // An answer that arrives late — the user cancelled,
                            // then picked something — has nothing left to
                            // unblock, and dropping it is what makes the cancel
                            // final rather than a race with the surface.
                            if let Some(waiter) = self.ask_waiters.lock().await.remove(&request_id) {
                                let _ = waiter.send(answer);
                            }
                        }
                        EngineCommand::SetBudget { tokens } => {
                            self.budget = tokens;
                            // A raised cap is a fresh start: the surface is
                            // free to spend again, and the next trip has to
                            // be reported again to mean anything.
                            self.budget_tripped = false;
                            let spent = self.spent.load(Ordering::SeqCst);
                            let _ = self.events.send(EngineEvent::BudgetUpdated { spent, limit: self.budget }).await;
                            if self.over_budget().await {
                                while let Some(text) = queued.pop_front() {
                                    let _ = self.events.send(EngineEvent::PromptReturned { text }).await;
                                }
                            }
                        }
                        EngineCommand::SetMoneyBudget { micro_usd } => {
                            // A cap the engine cannot enforce is refused, and
                            // the refusal names the model: an unpriced model
                            // is not a free one, so accepting the cap would
                            // promise a bound whose spend is invisible.
                            if micro_usd.is_some()
                                && self.resolver.price(&primary_model).is_none()
                            {
                                let _ = self.events.send(EngineEvent::MoneyBudgetUnpriced { model: primary_model.clone() }).await;
                                continue;
                            }
                            self.money_budget = micro_usd;
                            self.money_tripped = false;
                            self.money_unpriced_said.store(false, Ordering::SeqCst);
                            let spent_micro_usd = self.spent_micro_usd.load(Ordering::SeqCst);
                            let _ = self.events.send(EngineEvent::MoneyBudgetUpdated { spent_micro_usd, limit_micro_usd: self.money_budget }).await;
                            if self.over_budget().await {
                                while let Some(text) = queued.pop_front() {
                                    let _ = self.events.send(EngineEvent::PromptReturned { text }).await;
                                }
                            }
                        }
                        EngineCommand::SetMode { mode } => {
                            // The running turn keeps the tools it started
                            // with; the mode picks the tools of the next one.
                            self.mode = mode;
                            let _ = self.events.send(EngineEvent::ModeChanged { mode }).await;
                        }
                        EngineCommand::Shutdown => {
                            if let Some((_, aborted)) = active.take() {
                                aborted.store(true, Ordering::SeqCst);
                                self.config.interrupt.raise();
                            }
                            // The timers hold a command sender, so leaving
                            // them alive keeps the channel open forever. A
                            // handed-over command goes down too: the session
                            // that would report it is going away, so it must
                            // not outlive it as an orphan.
                            let draining: Vec<Job> = match self.jobs.lock() {
                                Ok(mut jobs) => jobs.drain().map(|(_, job)| job).collect(),
                                Err(_) => Vec::new(),
                            };
                            for job in draining {
                                match job {
                                    Job::Loop(job) => job.timer.abort(),
                                    Job::Command(job) => job.cancel.cancel(),
                                }
                            }
                            // The session's subagents go down with it: they
                            // hold claims and their events have nowhere to go
                            // once this loop ends, so leaving them running is
                            // the same orphan the jobs above are not allowed
                            // to become.
                            if let Some(agents) = &self.agents {
                                agents.stop_all().await;
                            }
                            break;
                        }
                        EngineCommand::ApproveTool { call_id, approved } => {
                            if let Some(waiter) = self.approval_waiters.lock().await.remove(&call_id) {
                                  let _ = waiter.send(approved);
                            }
                        }
                        EngineCommand::SpawnAgent { name, task, kind } => {
                            if let Some(agents) = &self.agents {
                                agents.spawn(name, task, kind).await;
                            } else {
                                self.emit_control_failure("agent supervisor is not configured").await;
                            }
                        }
                        EngineCommand::FocusAgent { agent_id } => {
                            let handled = match &self.agents {
                                Some(agents) => agents.focus(&agent_id).await,
                                None => false,
                            };
                            if !handled {
                                self.emit_control_failure("agent is not available").await;
                            }
                        }
                        EngineCommand::ReviveAgent { agent_id } => {
                            let handled = match &self.agents {
                                Some(agents) => agents.revive(&agent_id).await,
                                None => false,
                            };
                            if !handled {
                                self.emit_control_failure("agent cannot be revived").await;
                            }
                        }
                        EngineCommand::StopAgent { agent_id } => {
                            let handled = match &self.agents {
                                Some(agents) => agents.stop(&agent_id).await,
                                None => false,
                            };
                            if !handled {
                                self.emit_control_failure("agent is not available").await;
                            }
                        }
                        EngineCommand::DescribeContext => {
                            let parts = self.context_parts().await;
                            let window = self.config.context_window;
                            let _ = self.events.send(EngineEvent::ContextBreakdown { parts, window }).await;
                        }
                        EngineCommand::Compact { focus } => {
                            let turn_id = TurnId(self.next_turn.load(Ordering::SeqCst));
                            let running = active.is_some();
                            let event = compact_history(&mut self.config, focus.as_deref(), running, turn_id);
                            let _ = self.events.send(event).await;
                        }
                    }
                }
                completed = done_rx.recv() => {
                    if let Some(completed) = completed
                        && active.as_ref().is_some_and(|(id, _)| *id == completed.turn_id)
                    {
                        active = None;
                        // A rewind that landed while the turn ran replaced the
                        // history the turn was built on, so that turn's copy is
                        // stale and must not come back.
                        if let Some(history) = completed.history
                            && completed.epoch == self.history_epoch
                        {
                            self.config.restored_messages = history;
                        }
                        // The name comes from the finished turn, never from
                        // inside it: this returns before the namer has talked
                        // to anything.
                        self.name_session(primary_model.clone());
                        // A steer typed while the final answer streamed found
                        // no step boundary left in the turn. It was addressed
                        // to this conversation, so it runs next, ahead of the
                        // prompts queued behind the turn.
                        let unread = self.steering.drain();
                        if !unread.is_empty() {
                            let joined = unread.iter().map(SmolStr::as_str).collect::<Vec<_>>().join("\n\n");
                            queued.push_front(joined.into());
                        }
                        let spent = self.spent.load(Ordering::SeqCst);
                        let _ = self.events.send(EngineEvent::BudgetUpdated { spent, limit: self.budget }).await;
                        let spent_micro_usd = self.spent_micro_usd.load(Ordering::SeqCst);
                        let _ = self.events.send(EngineEvent::MoneyBudgetUpdated { spent_micro_usd, limit_micro_usd: self.money_budget }).await;
                        if self.over_budget().await {
                            // What was waiting behind this turn is handed
                            // back rather than run: the cap is reached, and
                            // a queue that drains anyway is not a cap.
                            while let Some(text) = queued.pop_front() {
                                let _ = self.events.send(EngineEvent::PromptReturned { text }).await;
                            }
                        } else if let Some(text) = queued.pop_front() {
                            let text = self.expand_skills(text).await;
                            let system = self.system_prompt().await;
                            active = Some(self.spawn_turn(text, primary_model.clone(), system, done_tx.clone()));
                        }
                    }
                }
            }
        }
    }

    /// Whether the session has spent a cap, reporting the first time it has.
    /// A capless session is never over budget.
    ///
    /// Two caps, one place: the token cap as it was, and the money cap beside
    /// it. Either one reaching its limit stops the next turn, and each reports
    /// its own event once — the token event's shape carries tokens, so the
    /// money cap has its own rather than a number wearing the wrong unit.
    async fn over_budget(&mut self) -> bool {
        let mut tripped = false;
        if let Some(limit) = self.budget {
            let spent = self.spent.load(Ordering::SeqCst);
            if spent >= limit {
                if !self.budget_tripped {
                    self.budget_tripped = true;
                    let _ = self
                        .events
                        .send(EngineEvent::BudgetExceeded { spent, limit })
                        .await;
                }
                tripped = true;
            }
        }
        if let Some(limit) = self.money_budget {
            let spent_micro_usd = self.spent_micro_usd.load(Ordering::SeqCst);
            if spent_micro_usd >= limit {
                if !self.money_tripped {
                    self.money_tripped = true;
                    let _ = self
                        .events
                        .send(EngineEvent::MoneyBudgetExceeded {
                            spent_micro_usd,
                            limit_micro_usd: limit,
                        })
                        .await;
                }
                tripped = true;
            }
        }
        tripped
    }

    /// Starts a background loop and reports it, or refuses it.
    ///
    /// The timer only ever queues a prompt; it never spawns a turn itself,
    /// so a loop firing during a live turn waits its place in the queue
    /// instead of racing the user.
    async fn start_loop(&mut self, interval_secs: u64, prompt: SmolStr) {
        if interval_secs == 0 {
            self.emit_control_failure("loop interval must be at least one second")
                .await;
            return;
        }
        if prompt.trim().is_empty() {
            self.emit_control_failure("loop needs a prompt").await;
            return;
        }
        let seq = self.next_job.fetch_add(1, Ordering::SeqCst);
        let id = SmolStr::from(format!("job-{seq}"));
        let runs = Arc::new(AtomicU64::new(0));
        let timer = {
            let commands = self.self_commands.clone();
            let prompt = prompt.clone();
            let runs = Arc::clone(&runs);
            tokio::spawn(async move {
                let period = std::time::Duration::from_secs(interval_secs);
                loop {
                    tokio::time::sleep(period).await;
                    if commands
                        .send(EngineCommand::FollowUp {
                            text: prompt.clone(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    runs.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        let job = LoopJob {
            seq,
            prompt,
            interval_secs,
            runs,
            timer,
        };
        let info = job.info(&id);
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.insert(id, Job::Loop(job));
        }
        let _ = self
            .events
            .send(EngineEvent::JobStarted { job: info })
            .await;
    }

    /// Every live job, in a stable order so the listing does not shuffle.
    fn job_list(&self) -> Vec<crate::protocol::JobInfo> {
        let Ok(jobs) = self.jobs.lock() else {
            return Vec::new();
        };
        let mut jobs: Vec<_> = jobs.iter().collect();
        jobs.sort_by_key(|(_, job)| job.seq());
        jobs.into_iter().map(|(id, job)| job.info(id)).collect()
    }

    /// Asks the advisor about the conversation, off the command loop.
    ///
    /// The consult is not a turn: it holds no history, takes no tools, and
    /// cannot queue behind or ahead of the user's work. Whatever comes back
    /// — an opinion or a reason there is none — reaches the surface.
    fn spawn_consult(&self, question: Option<SmolStr>, current_model: SmolStr) {
        let conversation = self.config.restored_messages.clone();
        let resolver = Arc::clone(&self.resolver);
        let events = self.events.clone();
        let agent_dir = self.config.agent_dir.clone();
        let workspace = self
            .config
            .workspace_root
            .clone()
            .or_else(|| self.config.genome_root.clone());
        tokio::spawn(async move {
            let model = advisor_model(agent_dir, workspace, current_model.clone())
                .await
                .unwrap_or(current_model);
            let advisor = crate::advisor::Advisor::new(resolver, model);
            let event = match advisor.consult(&conversation, question.as_deref()).await {
                Ok(text) => EngineEvent::AdvisorAnswer { text },
                Err(error) => EngineEvent::AdvisorFailed {
                    reason: error.to_string().into(),
                },
            };
            let _ = events.send(event).await;
        });
    }

    /// Hands a finished turn to the session namer.
    ///
    /// Everything here is a cheap local check — is there a session, is there
    /// anything to name it after — and the attempt itself lives in its own
    /// task. No model is called and no database is opened on this thread, so
    /// a slow, keyless or failing namer costs the command loop nothing.
    fn name_session(&self, model: SmolStr) {
        let (Some(agent_dir), Some(session_id)) = (
            self.config.agent_dir.clone(),
            self.config.session_id.clone(),
        ) else {
            return;
        };
        let Some(first_message) = crate::naming::first_user_message(&self.config.restored_messages)
        else {
            return;
        };
        let workspace = self
            .config
            .workspace_root
            .clone()
            .or_else(|| self.config.genome_root.clone())
            .unwrap_or_else(|| PathBuf::from("."));
        crate::naming::SessionNamer::new(
            Arc::clone(&self.resolver),
            self.events.clone(),
            agent_dir,
            session_id,
            workspace,
            model,
        )
        .spawn(first_message);
    }

    async fn emit_control_failure(&self, message: &str) {
        let _ = self
            .events
            .send(EngineEvent::Failed {
                turn_id: None,
                reason: ErrorReason::Rejected,
                message: message.into(),
            })
            .await;
    }

    /// Pull in the body of every skill the prompt names with `/name`.
    ///
    /// Only the bytes sent to the model change: the surface already logged
    /// and drew the text as typed. A refused body is reported instead of
    /// being pasted in, so a skill missing from a prompt is never silent.
    async fn expand_skills(&self, text: SmolStr) -> SmolStr {
        let cwd = self
            .config
            .workspace_root
            .clone()
            .or_else(|| self.config.genome_root.clone());
        let agent_dir = self.config.agent_dir.clone();
        let typed = text.clone();
        // Reads a directory and up to BODY_CAP per skill: off the runtime thread.
        let expanded = tokio::task::spawn_blocking(move || {
            crate::skills::expand(&typed, cwd.as_deref(), agent_dir.as_deref())
        })
        .await;
        let Ok(expanded) = expanded else {
            return text;
        };
        for notice in expanded.notices {
            let _ = self
                .events
                .send(EngineEvent::Notice {
                    message: notice.into(),
                })
                .await;
        }
        expanded.text.map(SmolStr::from).unwrap_or(text)
    }

    /// Identity and personality, then project rules and skill names.
    ///
    /// Everything here is static for the life of the session, and that is
    /// the point: this text sits in front of the whole conversation, so a
    /// single byte moving in it invalidates the provider's cache for every
    /// token behind it. The parts rebuilt every turn — recalled memory, the
    /// genome map, and the working-tree diff — ride on the newest user
    /// message instead (see [`prompt_with_context`]).
    ///
    /// A missing agent directory degrades to whatever is left rather than
    /// failing the turn. Project rules and skills are their own sections:
    /// they are not folded into `SOUL.md`. Skill bodies are not injected.
    async fn system_prompt(&self) -> Option<SmolStr> {
        let mut parts = Vec::new();
        if let Some(identity) = self.identity_prompt() {
            parts.push(identity.to_string());
        }
        // Duck mode is repo-blind: project rules and the skill list both
        // describe this repository, and a partner that quotes them has seen
        // it after all.
        if self.mode != crate::protocol::SessionMode::Duck {
            if let Some(project) = self.project_context() {
                parts.push(project);
            }
            if let Some(skills) = self.skill_list() {
                parts.push(skills);
            }
        }
        if let Some(brief) = mode_brief(self.mode) {
            parts.push(brief.to_owned());
        }
        let joined = parts.join("\n\n");
        if joined.is_empty() {
            None
        } else {
            Some(joined.into())
        }
    }

    /// The registry this mode's turns get.
    ///
    /// Both restricted modes pick by tier, so every tier — the network one
    /// included — is a decision one of the two tables made on purpose,
    /// rather than whatever a "not a read" catch-all happened to do. See
    /// [`PLAN_TIERS`] and [`DUCK_TIERS`] for which tier each keeps and why.
    /// A tool the model was never handed is one it cannot reach for by
    /// mistake.
    fn mode_tools(&self) -> ToolRegistry {
        let mut tools = self.tools.clone();
        match self.mode {
            crate::protocol::SessionMode::Agent => {}
            crate::protocol::SessionMode::Plan => tools.retain_tiers(PLAN_TIERS),
            crate::protocol::SessionMode::Duck => tools.retain_tiers(DUCK_TIERS),
        }
        tools
    }

    /// `AGENTS.md` from the workspace and the agent directory. Flagged files
    /// are already omitted. A missing workspace still contributes the user file.
    fn project_context(&self) -> Option<String> {
        let cwd = self
            .config
            .workspace_root
            .as_deref()
            .or(self.config.genome_root.as_deref());
        crate::project_context::render(
            cwd,
            self.config.agent_dir.as_deref(),
            process_home().as_deref(),
        )
    }

    /// Name and description of discovered skills. Bodies stay on disk.
    fn skill_list(&self) -> Option<String> {
        let cwd = self
            .config
            .workspace_root
            .as_deref()
            .or(self.config.genome_root.as_deref());
        crate::skills::render(cwd, self.config.agent_dir.as_deref())
    }

    /// The memories relevant to this turn. The turn itself builds the copy
    /// the model sees; this one only measures.
    async fn recalled_memory(&self) -> Option<SmolStr> {
        recalled_memory(self.config.agent_dir.clone(), &self.touched).await
    }

    /// Soul and personality. Memory is no longer pasted in whole: the index
    /// recalls the rows this turn needs, which is the only copy the model sees.
    fn identity_prompt(&self) -> Option<SmolStr> {
        let agent_dir = self.config.agent_dir.clone()?;
        let built = titi_soul::SystemPromptBuilder::build(&agent_dir, None, None).ok()?;
        Some(built.render().into())
    }

    /// This turn's map. As with the recall, the turn builds the copy the
    /// model sees and this one only measures.
    async fn genome_system(&self) -> Option<SmolStr> {
        genome_map(
            self.config.genome_root.clone(),
            self.config.genome_limit,
            self.genome.as_ref(),
            &self.touched,
            GENOME_QUIESCE,
        )
        .await
    }

    /// What fills the context window right now, one part per piece a turn
    /// assembles, measured with the estimator that feeds `ContextUsage`.
    ///
    /// The genome map and the recalled memory are their own parts on
    /// purpose: they are rebuilt every turn, so they are both the largest
    /// moving weight and the reason a prompt cache misses.
    async fn context_parts(&self) -> Vec<ContextPart> {
        let tools = self
            .tools
            .specs()
            .iter()
            .map(|spec| {
                titi_core::compaction::estimate_tokens(&spec.name)
                    + titi_core::compaction::estimate_tokens(&spec.description)
                    + titi_core::compaction::estimate_tokens(&spec.parameters.to_string())
            })
            .sum();
        vec![
            context_part("system prompt", self.identity_prompt().as_deref()),
            context_part("project rules", self.project_context().as_deref()),
            context_part("skills", self.skill_list().as_deref()),
            context_part("recalled memory", self.recalled_memory().await.as_deref()),
            context_part("genome map", self.genome_system().await.as_deref()),
            ContextPart {
                label: "history".into(),
                tokens: crate::compaction::estimate_request(&self.config.restored_messages),
            },
            ContextPart {
                label: "tool specs".into(),
                tokens: tools,
            },
        ]
    }

    fn spawn_turn(
        &self,
        prompt: SmolStr,
        primary_model: SmolStr,
        system: Option<SmolStr>,
        done: mpsc::Sender<TurnDone>,
    ) -> (TurnId, Arc<AtomicBool>) {
        let epoch = self.history_epoch;
        let turn_id = TurnId(self.next_turn.fetch_add(1, Ordering::SeqCst));
        // A cancel is spent once a new turn starts. The command it was raised
        // for still sees it: the interrupt counts raises, not just the flag.
        self.config.interrupt.clear();
        let aborted = Arc::new(AtomicBool::new(false));
        let task_abort = Arc::clone(&aborted);
        let mut config = self.config.clone();
        if self.mode == crate::protocol::SessionMode::Duck {
            // No ranked file list rides the prompt: a duck that can quote
            // the repository is not repo-blind, whatever the brief says.
            config.genome_root = None;
            // `mode_tools` left this turn [`DUCK_TIERS`] and nothing else, so
            // everything it can call is network tier: reach that stops
            // outside this machine. Asking the user to approve the one thing
            // the mode exists to do is noise on a surface that can show the
            // prompt and a hang on one that cannot, so this turn approves
            // what it was handed. Only this clone is loosened; the session's
            // own approval mode is untouched and an agent turn still asks.
            config.approval_mode = ApprovalMode::Yolo;
        }
        let resolver = Arc::clone(&self.resolver);
        let events = self.events.clone();
        let tools = self.mode_tools();
        let waiters = Arc::clone(&self.approval_waiters);
        let trajectory = Arc::clone(&self.trajectory);
        let touched = Arc::clone(&self.touched);
        let genome = self.genome.clone();
        let claims = self.claims.clone();
        let steering = self.steering.clone();
        let spent = Arc::clone(&self.spent);
        let money = MoneyLedger {
            spent_micro_usd: Arc::clone(&self.spent_micro_usd),
            unpriced_said: Arc::clone(&self.money_unpriced_said),
        };
        // Whether a money cap is in force when the turn starts. The cap can
        // move mid-turn, but what a turn needs it for is whether to say that
        // it cannot be measured — and the next turn says it again if the cap
        // arrived late.
        let money_bounded = self.money_budget.is_some();
        tokio::spawn(async move {
            let history = run_turn(
                turn_id,
                prompt,
                primary_model,
                system,
                config,
                resolver,
                events,
                task_abort,
                tools,
                waiters,
                trajectory,
                touched,
                genome,
                claims,
                steering,
                spent,
                money,
                money_bounded,
            )
            .await;
            let _ = done
                .send(TurnDone {
                    turn_id,
                    history,
                    epoch,
                })
                .await;
        });
        (turn_id, aborted)
    }

    /// `/goal` runs the existing loop on the session model. It does not
    /// replace the active turn or queue a `SubmitPrompt`.
    fn spawn_goal(&self, text: SmolStr, model: SmolStr) {
        let events = self.events.clone();
        let text = text.trim().to_owned();
        if text.is_empty() {
            tokio::spawn(async move {
                let _ = events
                    .send(EngineEvent::GoalFinished {
                        report: "usage: /goal <text>".into(),
                    })
                    .await;
            });
            return;
        }
        let coder = Arc::new(crate::RunnerCoder::new(Arc::new(
            crate::StreamingAgentRunner::new(Arc::clone(&self.resolver), model.clone()),
        )));
        let reviewer = Arc::new(crate::AgentReviewer::new(
            Arc::new(crate::StreamingAgentRunner::new(
                Arc::clone(&self.resolver),
                model,
            )),
            "reviewer",
        ));
        let gates = crate::CommandGates::new(self.config.goal_gates.clone());
        let gates: Arc<dyn crate::Gates> = Arc::new(match &self.config.workspace_root {
            Some(root) => gates.in_dir(root.clone()),
            None => gates,
        });
        tokio::spawn(async move {
            let outcome = crate::GoalLoop::new(coder, reviewer)
                .with_gates(gates)
                .run(text)
                .await;
            let _ = events
                .send(EngineEvent::GoalFinished {
                    report: crate::goal_report(&outcome).into(),
                })
                .await;
        });
    }

    /// `/council` seats [`crate::DEFAULT_BRIEFS`] on the session model: one
    /// runner per member, one for the fold. Like `/goal` it does not replace
    /// the active turn or queue a `SubmitPrompt`.
    fn spawn_council(&self, question: SmolStr, model: SmolStr) {
        let events = self.events.clone();
        let question = question.trim().to_owned();
        if question.is_empty() {
            tokio::spawn(async move {
                let _ = events
                    .send(EngineEvent::CouncilFinished {
                        report: "usage: /council <question>".into(),
                    })
                    .await;
            });
            return;
        }
        let members = crate::DEFAULT_BRIEFS
            .iter()
            .map(|(name, brief, effort)| {
                crate::CouncilMember::new(
                    *name,
                    *brief,
                    model.clone(),
                    *effort,
                    Arc::new(crate::StreamingAgentRunner::new(
                        Arc::clone(&self.resolver),
                        model.clone(),
                    )),
                )
            })
            .collect();
        let synthesizer = Arc::new(crate::StreamingAgentRunner::new(
            Arc::clone(&self.resolver),
            model,
        ));
        tokio::spawn(async move {
            let report = match crate::run_council(members, synthesizer, question).await {
                Ok(report) => crate::council_report(&report),
                Err(error) => format!("council: {error}"),
            };
            let _ = events
                .send(EngineEvent::CouncilFinished {
                    report: report.into(),
                })
                .await;
        });
    }

    /// The built-in graph: a council decides the approach, the goal loop does
    /// the work, and a goal that does not pass goes round once more before
    /// the cap stops it. Like `/goal` and `/council` it runs beside the turn
    /// rather than replacing it.
    fn spawn_graph(&self, task: SmolStr, model: SmolStr) {
        let events = self.events.clone();
        let task = task.trim().to_owned();
        if task.is_empty() {
            tokio::spawn(async move {
                let _ = events
                    .send(EngineEvent::GraphFinished {
                        report: "usage: /graph <task>".into(),
                    })
                    .await;
            });
            return;
        }
        let runner = |model: SmolStr| {
            Arc::new(crate::StreamingAgentRunner::new(
                Arc::clone(&self.resolver),
                model,
            ))
        };
        let members = crate::DEFAULT_BRIEFS
            .iter()
            .map(|(name, brief, effort)| {
                crate::CouncilMember::new(
                    *name,
                    *brief,
                    model.clone(),
                    *effort,
                    runner(model.clone()),
                )
            })
            .collect();
        let coder = Arc::new(crate::RunnerCoder::new(runner(model.clone())));
        let reviewer = Arc::new(crate::AgentReviewer::new(runner(model.clone()), "reviewer"));
        let gates = crate::CommandGates::new(self.config.goal_gates.clone());
        let gates: Arc<dyn crate::Gates> = Arc::new(match &self.config.workspace_root {
            Some(root) => gates.in_dir(root.clone()),
            None => gates,
        });
        let nodes = vec![
            crate::Node::new(
                "council",
                crate::Job::Council {
                    members,
                    synthesizer: runner(model.clone()),
                    question: format!(
                        "How should this be done, and what is the first step?\n\n{task}"
                    )
                    .into(),
                },
            )
            .then("goal"),
            crate::Node::new(
                "goal",
                crate::Job::Goal {
                    coder,
                    reviewer,
                    gates: Some(gates),
                    goal: task.into(),
                },
            )
            .with_max_runs(2)
            .on(crate::Gate::On(crate::Verdict::Pass), crate::Step::Done)
            .on(crate::Gate::NotPass, crate::Step::To("goal".into())),
        ];
        tokio::spawn(async move {
            let report = match crate::run_graph(nodes).await {
                Ok(run) => crate::graph_report(&run),
                Err(error) => format!("graph: {error}"),
            };
            let _ = events
                .send(EngineEvent::GraphFinished {
                    report: report.into(),
                })
                .await;
        });
    }
}

fn process_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn context_part(label: &str, text: Option<&str>) -> ContextPart {
    ContextPart {
        label: label.into(),
        tokens: text
            .map(titi_core::compaction::estimate_tokens)
            .unwrap_or(0),
    }
}

/// Manual compaction: fold the replayed history now, whatever the threshold
/// says, and report what happened.
///
/// A running turn built its request on this history and hands its own copy
/// back when it ends, so folding underneath it would either be overwritten
/// or cost that turn its answer. Manual compaction waits for it instead.
///
/// `focus` is what the user asked to keep: the lines of the folded prefix
/// that mention it are appended to the digest, so the thread they named is
/// not the part the fold takes away.
fn compact_history(
    config: &mut EngineConfig,
    focus: Option<&str>,
    turn_running: bool,
    turn_id: TurnId,
) -> EngineEvent {
    if turn_running {
        return EngineEvent::Notice {
            message: "compact: a turn is running, try again when it finishes".into(),
        };
    }
    let forced = titi_core::compaction::CompactionPolicy {
        threshold_percent: 0.0,
        ..config.compaction.clone()
    };
    let before = focus.map(|_| config.restored_messages.clone());
    let Some(done) = crate::compaction::compact(
        &mut config.restored_messages,
        &forced,
        config.context_window.max(1),
    ) else {
        return EngineEvent::Notice {
            message: "compact: nothing to fold yet".into(),
        };
    };
    if let (Some(focus), Some(before)) = (focus, before)
        && let Some(digest) = config.restored_messages.first_mut()
    {
        let folded: Vec<&str> = before
            .iter()
            .take(done.folded)
            .map(|message| message.content.as_str())
            .collect();
        let kept = titi_core::compaction::focus_digest(focus, &folded);
        digest.content = format!("{}\n{kept}", digest.content).into();
    }
    EngineEvent::Compacted {
        turn_id,
        folded: done.folded as u32,
        tokens_before: done.tokens_before,
        strategy: done.strategy,
    }
}

/// What a finished turn hands back to the command loop.
struct TurnDone {
    turn_id: TurnId,
    /// The history the next turn must start from, without the system prompt
    /// (rebuilt per turn). `None` when the turn did not finish cleanly.
    history: Option<Vec<ChatMessage>>,
    /// The value of `EngineRuntime::history_epoch` when the turn started.
    epoch: u64,
}

/// Returns the visible history to keep, or `None` when this turn must leave
/// the history untouched.
async fn run_turn(
    turn_id: TurnId,
    prompt: SmolStr,
    primary_model: SmolStr,
    system: Option<SmolStr>,
    config: EngineConfig,
    resolver: Arc<dyn TransportResolver>,
    events: mpsc::Sender<EngineEvent>,
    aborted: Arc<AtomicBool>,
    tools: ToolRegistry,
    waiters: ApprovalWaiters,
    trajectory: TrajectorySink,
    touched: TouchedSink,
    genome: Option<GenomeHandle>,
    claims: Claims,
    steering: Steering,
    // Session-wide token meter the turn adds its own spend to.
    spent: Arc<AtomicU64>,
    // Session-wide money ledger, and whether a cap is over it: see
    // `MoneyLedger`.
    money: MoneyLedger,
    money_bounded: bool,
) -> Option<Vec<ChatMessage>> {
    // In front of the resolve below, and outside the synchronous ladder: a
    // subscription token that expires between turns would otherwise fail the
    // turn it was resolved for. The outcomes are the resolver's business —
    // a refresh that failed leaves the stored credential in place, and the
    // resolve that follows says what is wrong with it.
    let _ = resolver.refresh_due().await;
    let mut models = Vec::with_capacity(1 + config.fallback_models.len());
    models.push(primary_model);
    models.extend(config.fallback_models);
    let mut previous_model: Option<SmolStr> = None;
    // Built once for the turn, in front of the prompt it belongs to. A
    // fallback model reuses it: rebuilding per attempt would send two
    // different requests for one question. The diff is captured first so
    // its paths are in the touched set the map reads.
    // Duck mode is repo-blind: a diff is the repository, the same way the
    // map is. Plan mode still sees it — a plan about an edit needs the edit.
    let snapshot = if config.mode == crate::protocol::SessionMode::Duck {
        None
    } else {
        working_tree_diff(config.workspace_root.clone(), config.sensitive.clone()).await
    };
    if let Some(snapshot) = &snapshot {
        let mut edited = touched.lock().await;
        for path in &snapshot.files {
            edited.insert(path.clone());
        }
    }
    let recalled = recalled_memory(config.agent_dir.clone(), &touched).await;
    let genome_text = genome_map(
        config.genome_root.clone(),
        config.genome_limit,
        genome.as_ref(),
        &touched,
        GENOME_QUIESCE,
    )
    .await;
    let contextual_prompt = prompt_with_context(
        &prompt,
        recalled.as_deref(),
        genome_text.as_deref(),
        snapshot.as_ref().map(|shot| shot.text.as_str()),
    );

    // Per turn, not per model: rounds a fallback leaves behind were paid for.
    let mut meter = TurnMeter::new(&spent, &money);
    let history = async {
        // What the last model to give up said, for the failure that ends the
        // turn when every model has: "unavailable" alone does not say why.
        let mut last_failure: Option<String> = None;
        for model in models {
            if aborted.load(Ordering::SeqCst) {
                return None;
            }
            if let Some(previous) = previous_model.take() {
                let _ = events
                    .send(EngineEvent::ModelSwitched {
                        turn_id: Some(turn_id),
                        from: previous,
                        to: model.clone(),
                    })
                    .await;
            }
            let resolved = match resolver.resolve(&model) {
                Ok(resolved) => resolved,
                Err(error) => {
                    meter.settle(&events, turn_id).await;
                    let _ = events
                        .send(EngineEvent::Failed {
                            turn_id: Some(turn_id),
                            reason: ErrorReason::Rejected,
                            message: error.to_string().into(),
                        })
                        .await;
                    return None;
                }
            };
            let credential = resolved.credential;
            let wire_model = resolved.wire_model;
            let transport = resolved.transport;
            // The price of *this* model: a fallback is a different price, not
            // the same one, and the meter charges each round at its own rate.
            let price = resolver.price(&model);
            if price.is_none() && money_bounded && !money.unpriced_said.swap(true, Ordering::SeqCst)
            {
                // The cap stays in force; what changes is that the engine can
                // no longer measure against it, and says so rather than
                // letting the bound look enforced.
                let _ = events
                    .send(EngineEvent::MoneyBudgetUnpriced {
                        model: model.clone(),
                    })
                    .await;
            }
            meter.for_model(price);
            let _ = events
                .send(EngineEvent::TurnStarted {
                    turn_id,
                    model: model.clone(),
                })
                .await;

            if let Some(recorder) = trajectory.lock().await.as_mut() {
                let _ = recorder.record(titi_core::trajectory::EventKind::UserMessage {
                    text: prompt.to_string(),
                });
            }
            let mut messages = Vec::new();
            if let Some(system) = &system {
                messages.push(ChatMessage {
                    role: Role::System,
                    content: system.clone(),
                    tool_calls: Vec::new(),
                    ..Default::default()
                });
            }
            // A resumed session replays its history before the new prompt.
            messages.extend(config.restored_messages.iter().cloned());
            messages.push(ChatMessage {
                role: Role::User,
                content: contextual_prompt.clone(),
                tool_calls: Vec::new(),
                ..Default::default()
            });
            let mut tool_rounds = 0;
            loop {
                // `execute_tools` returns as soon as the abort lands, and the loop
                // used to walk straight back into another stream. Nothing below
                // this point is worth doing for a turn nobody will read.
                if aborted.load(Ordering::SeqCst) {
                    return None;
                }
                // Anything typed mid-flight lands before the next attempt.
                for text in steering.drain() {
                    messages.push(ChatMessage {
                        role: Role::User,
                        content: text,
                        tool_calls: Vec::new(),
                        ..Default::default()
                    });
                }
                // A turn accumulates tool results without bound; fold the oldest
                // away before the provider refuses the request.
                if let Some(folded) = crate::compaction::compact(
                    &mut messages,
                    &config.compaction,
                    config.context_window,
                ) {
                    if let Some(recorder) = trajectory.lock().await.as_mut() {
                        let _ = recorder.record(titi_core::trajectory::EventKind::Compaction {
                            folded: folded.folded as u64,
                            strategy: folded.strategy.to_string(),
                        });
                    }
                    let _ = events
                        .send(EngineEvent::Compacted {
                            turn_id,
                            folded: folded.folded as u32,
                            tokens_before: folded.tokens_before,
                            strategy: folded.strategy.clone(),
                        })
                        .await;
                }
                let _ = events
                    .send(EngineEvent::ContextUsage {
                        turn_id,
                        tokens: crate::compaction::estimate_request(&messages),
                        window: config.context_window,
                    })
                    .await;
                let mut last_error = None;
                let mut completed = false;
                // One fold per turn for a request the provider rejected as too
                // long: the window does not change, so a second rejection is
                // the answer.
                let mut folded_for_length = false;
                for attempt in 0..=config.max_transient_retries {
                    // A stall already waited out its own timeout; a rate limit or
                    // an outage is asked again only after a pause.
                    if let Some(wait) =
                        retry_wait(last_error.as_ref(), config.retry_backoff, attempt)
                        && !back_off(wait, &aborted).await
                    {
                        return None;
                    }
                    match stream_attempt(
                        turn_id,
                        &messages,
                        &wire_model,
                        Arc::clone(&transport),
                        credential.clone(),
                        events.clone(),
                        Arc::clone(&aborted),
                        &tools,
                        &mut meter,
                    )
                    .await
                    {
                        Ok((text, calls, thinking)) if calls.is_empty() => {
                            if !text.is_empty() {
                                messages.push(ChatMessage {
                                    role: Role::Assistant,
                                    content: text,
                                    tool_calls: Vec::new(),
                                    thinking,
                                    ..Default::default()
                                });
                            }
                            completed = true;
                            break;
                        }
                        Ok((text, calls, thinking)) => {
                            if tool_rounds >= config.max_tool_rounds {
                                meter.settle(&events, turn_id).await;
                                let _ = events
                                    .send(EngineEvent::Failed {
                                        turn_id: Some(turn_id),
                                        reason: ErrorReason::Rejected,
                                        message: "tool round cap reached".into(),
                                    })
                                    .await;
                                return None;
                            }
                            tool_rounds += 1;
                            let extra = execute_tools(
                                turn_id,
                                calls,
                                text,
                                thinking,
                                &tools,
                                config.approval_mode,
                                &waiters,
                                &events,
                                &aborted,
                                &trajectory,
                                &touched,
                                // No genome root means no index to feed: a
                                // write has nowhere to go. The handle's
                                // publish point is handed over rather than
                                // the handle, because the tool loop folds the
                                // write in synchronously — the turn's own
                                // prompt must see it.
                                genome
                                    .as_ref()
                                    .filter(|_| config.genome_root.is_some())
                                    .map(GenomeHandle::shared),
                                &claims,
                                &MAIN_AGENT,
                                config.mask_ips,
                            )
                            .await;
                            messages.extend(extra);
                            last_error = None;
                            break;
                        }
                        Err((error, visible_output)) => {
                            if aborted.load(Ordering::SeqCst) {
                                return None;
                            }
                            // The request was longer than the model's window.
                            // Fold once and ask again: a provider's own words
                            // say what it refused, and the fold is the one
                            // thing that can change the request's length.
                            if !folded_for_length
                                && error.is_context_too_long()
                                && !visible_output
                            {
                                folded_for_length = true;
                                let forced = titi_core::compaction::CompactionPolicy {
                                    threshold_percent: 0.0,
                                    ..config.compaction.clone()
                                };
                                let _ = crate::compaction::compact(
                                    &mut messages,
                                    &forced,
                                    config.context_window.max(1),
                                );
                                last_error = None;
                                continue;
                            }
                            if visible_output || !error.is_retryable() {
                                meter.settle(&events, turn_id).await;
                                emit_transport_failure(
                                    &events,
                                    turn_id,
                                    error.naming_the_window(config.context_window),
                                )
                                .await;
                                return None;
                            }
                            last_error = Some(error);
                        }
                    }
                }
                if completed {
                    if let Some(recorder) = trajectory.lock().await.as_mut() {
                        let _ = recorder.record(titi_core::trajectory::EventKind::TurnEnd);
                    }
                    // A cancelled turn contributes nothing. Its assistant output is
                    // partial and its last tool calls may have no results, and a
                    // tool call without its result is a request no provider accepts.
                    if aborted.load(Ordering::SeqCst) {
                        return None;
                    }
                    return Some(visible_history(messages, system.as_ref()));
                }
                if let Some(error) = last_error {
                    last_failure = Some(format!("{model}: {error}"));
                    previous_model = Some(model.clone());
                    break;
                }
            }
        }

        meter.settle(&events, turn_id).await;
        let _ = events
            .send(EngineEvent::Failed {
                turn_id: Some(turn_id),
                reason: ErrorReason::Connection,
                message: match last_failure {
                    Some(last) => {
                        format!("all configured models are unavailable; last, {last}").into()
                    }
                    None => "all configured models are unavailable".into(),
                },
            })
            .await;
        None
    }
    .await;
    // A cancelled turn ends here without a word of its own.
    meter.settle(&events, turn_id).await;
    history
}

/// Longest pause before one transient retry.
pub const MAX_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(8);

/// The pause before attempt `attempt` (1 is the first retry): `base`, then
/// doubled each time, capped.
/// How long to wait before the next attempt.
///
/// What the provider asked for in a `retry-after`-family header wins; with
/// nothing asked for, the caller's own doubling schedule applies. `None` means
/// this error is not one to pause for at all (a stall already waited out its
/// own timeout).
fn retry_wait(
    last_error: Option<&TransportError>,
    base: std::time::Duration,
    attempt: u32,
) -> Option<std::time::Duration> {
    let TransportError::Retryable { retry_after, .. } = last_error? else {
        return None;
    };
    Some(retry_after.unwrap_or_else(|| retry_delay(base, attempt)))
}

fn retry_delay(base: std::time::Duration, attempt: u32) -> std::time::Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    base.saturating_mul(1 << doublings).min(MAX_RETRY_BACKOFF)
}

/// Sleeps for `delay` in short slices so a cancel is not kept waiting for
/// the whole pause. Returns `false` when the turn was cancelled meanwhile.
async fn back_off(delay: std::time::Duration, aborted: &AtomicBool) -> bool {
    const SLICE: std::time::Duration = std::time::Duration::from_millis(50);
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if aborted.load(Ordering::SeqCst) {
            return false;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return true;
        }
        tokio::time::sleep((deadline - now).min(SLICE)).await;
    }
}

/// The session's money, shared with the turns that spend it.
///
/// `spent_micro_usd` is summed by the turns from the same per-turn figure they
/// put on [`EngineEvent::TurnUsage`], so the ledger and a footer read one
/// number rather than two computations of it. `unpriced_said` is the
/// transition flag for [`EngineEvent::MoneyBudgetUnpriced`]: the turn that
/// finds out sets it, and only the first one reports, because a session that
/// keeps running on an unpriced model under a money cap does not need telling
/// every turn.
#[derive(Clone, Default)]
struct MoneyLedger {
    spent_micro_usd: Arc<AtomicU64>,
    unpriced_said: Arc<AtomicBool>,
}

/// What one turn has spent, round by round. A tool round re-sends the whole
/// conversation and is paid for like any other request, so the turn's usage
/// is the sum of its rounds; each round also goes on the session's meter,
/// which the budget reads.
struct TurnMeter<'a> {
    session: &'a Arc<AtomicU64>,
    prompt: u64,
    completion: u64,
    cached: u64,
    /// The session's money ledger, which this turn adds to as it reports.
    money: &'a MoneyLedger,
    /// The price of the model the current attempt is running on. `None` is
    /// *unpriced*: the turn's money is then not a zero, it is unknown.
    price: Option<ModelPrice>,
    /// What the turn has cost so far, in micro-dollars, over the rounds that
    /// had a price.
    turn_micro_usd: u64,
    /// A round ran on a model with no price, so the figure above is not the
    /// turn's whole cost and must not be presented as if it were.
    unpriced: bool,
    /// The turn's usage went out. It goes out once, however the turn ends.
    reported: bool,
}

impl<'a> TurnMeter<'a> {
    fn new(session: &'a Arc<AtomicU64>, money: &'a MoneyLedger) -> Self {
        Self {
            session,
            prompt: 0,
            completion: 0,
            cached: 0,
            money,
            price: None,
            turn_micro_usd: 0,
            unpriced: false,
            reported: false,
        }
    }

    /// The price of the model the next round runs on, which the turn sets per
    /// attempt: a fallback model is a different price, not the same one.
    fn for_model(&mut self, price: Option<ModelPrice>) {
        self.price = price;
    }

    /// The turn's usage, which counts as reported from here on.
    fn usage(&mut self, turn_id: TurnId) -> EngineEvent {
        self.reported = true;
        EngineEvent::TurnUsage {
            turn_id,
            prompt_tokens: u32::try_from(self.prompt).unwrap_or(u32::MAX),
            completion_tokens: u32::try_from(self.completion).unwrap_or(u32::MAX),
            cached_tokens: u32::try_from(self.cached).unwrap_or(u32::MAX),
            // A turn any of whose rounds went unpriced has no complete figure
            // to state, and a partial one would read as the whole of it.
            cost_micro_usd: (!self.unpriced).then_some(self.turn_micro_usd),
        }
    }

    /// Reports what the turn spent if nothing has yet. A turn that fails or
    /// is cancelled paid for every round it finished, and a session total
    /// that skips them undercounts.
    async fn settle(&mut self, events: &mpsc::Sender<EngineEvent>, turn_id: TurnId) {
        if !self.reported && (self.prompt > 0 || self.completion > 0) {
            let _ = events.send(self.usage(turn_id)).await;
        }
    }

    fn charge(&mut self, prompt: u64, completion: u64, cached: u64) {
        self.prompt = self.prompt.saturating_add(prompt);
        self.cached = self.cached.saturating_add(cached);
        self.completion = self.completion.saturating_add(completion);
        self.session
            .fetch_add(prompt.saturating_add(completion), Ordering::SeqCst);
        // The money is the same arithmetic a footer does, over the same three
        // counts, from the one place a price can come from. An unpriced round
        // adds nothing and says so rather than adding a zero: the session's
        // figure stays a floor and the turn reports no figure at all.
        match self.price {
            Some(price) => {
                let micro = price.cost_micro_usd(
                    u32::try_from(prompt).unwrap_or(u32::MAX),
                    u32::try_from(cached).unwrap_or(u32::MAX),
                    u32::try_from(completion).unwrap_or(u32::MAX),
                );
                self.turn_micro_usd = self.turn_micro_usd.saturating_add(micro);
                self.money
                    .spent_micro_usd
                    .fetch_add(micro, Ordering::SeqCst);
            }
            None => self.unpriced = true,
        }
    }
}

/// The turn's messages without the system prompt it was given: the next turn
/// rebuilds that itself. A digest compaction put in its place is not the
/// system prompt and stays, so folded messages do not come back.
fn visible_history(mut messages: Vec<ChatMessage>, system: Option<&SmolStr>) -> Vec<ChatMessage> {
    if let Some(system) = system
        && messages
            .first()
            .is_some_and(|first| first.role == Role::System && first.content == *system)
    {
        messages.remove(0);
    }
    messages
}

async fn stream_attempt(
    turn_id: TurnId,
    messages: &[ChatMessage],
    model: &SmolStr,
    transport: Arc<dyn Transport>,
    credential: Option<Credential>,
    events: mpsc::Sender<EngineEvent>,
    aborted: Arc<AtomicBool>,
    tools: &ToolRegistry,
    // The turn's meter, which adds this request to the session's as well.
    meter: &mut TurnMeter<'_>,
) -> Result<
    (
        SmolStr,
        Vec<crate::tool_loop::PendingToolCall>,
        Vec<titi_providers::ThinkingBlock>,
    ),
    (TransportError, bool),
> {
    // Every request the turn makes goes through here, including each transient
    // retry, so this is the one place that can be the last look at the flag
    // before bytes leave. A cancel that lands after `stream` was called hits a
    // request already in flight; the transport reads the same flag through
    // `RequestCtx::aborted` and drops a silent read on a short tick, so the
    // turn still ends promptly instead of waiting out the socket.
    if aborted.load(Ordering::SeqCst) {
        return Ok((SmolStr::default(), Vec::new(), Vec::new()));
    }
    let mut request = WireRequest::new(model.clone());
    request.messages = messages.to_vec();
    request.tools = tools.specs();
    let context = RequestCtx {
        credential,
        aborted: Arc::clone(&aborted),
    };
    let mut stream = transport
        .stream(request, context)
        .await
        .map_err(|error| (error, false))?;
    let mut visible_output = false;
    let mut collector = ToolCallCollector::default();
    let mut answer = String::new();
    let mut reported: Option<TokenUsage> = None;

    while let Some(event) = stream.next().await {
        if aborted.load(Ordering::SeqCst) {
            return Ok((SmolStr::default(), Vec::new(), Vec::new()));
        }
        visible_output |= event.is_visible_output();
        collector.observe(&event);
        match event {
            StreamEvent::TextDelta { text, .. } => {
                answer.push_str(&text);
                let _ = events
                    .send(EngineEvent::StreamDelta { turn_id, text })
                    .await;
            }
            StreamEvent::ThinkingDelta { text, .. } => {
                let _ = events
                    .send(EngineEvent::ThinkingDelta { turn_id, text })
                    .await;
            }
            StreamEvent::Usage(usage) => reported = Some(usage),
            StreamEvent::Done { reason } => {
                let calls = collector.take();
                // Every round is paid for, tool rounds included, so the
                // meter is bumped here rather than once per turn. The
                // provider's own count wins; a provider that reports none
                // (or a malformed one) is charged the project's estimate.
                match reported {
                    Some(usage) => meter.charge(
                        usage.prompt_tokens,
                        usage.completion_tokens,
                        usage.cached_tokens,
                    ),
                    None => meter.charge(
                        crate::compaction::estimate_request(messages),
                        titi_core::compaction::estimate_tokens(&answer),
                        0,
                    ),
                }
                if calls.is_empty() {
                    let _ = events.send(meter.usage(turn_id)).await;
                    let _ = events
                        .send(EngineEvent::TurnFinished { turn_id, reason })
                        .await;
                }
                return Ok((answer.into(), calls, collector.thinking().to_vec()));
            }
            StreamEvent::Error { reason, message } => {
                let error = if reason == ErrorReason::Connection && !visible_output {
                    TransportError::Retryable {
                        status: None,
                        message,
                        retry_after: None,
                    }
                } else {
                    // A stream error carries no status and no window: the
                    // provider said what it said, in words.
                    TransportError::Fatal {
                        status: None,
                        message,
                        context_too_long: false,
                    }
                };
                return Err((error, visible_output));
            }
            _ => {}
        }
    }

    Err((
        TransportError::Retryable {
            status: None,
            message: "stream ended without terminal event".into(),
            retry_after: None,
        },
        visible_output,
    ))
}

async fn emit_transport_failure(
    events: &mpsc::Sender<EngineEvent>,
    turn_id: TurnId,
    error: TransportError,
) {
    let reason = match error {
        TransportError::Fatal { .. } => ErrorReason::Rejected,
        TransportError::Retryable { .. } | TransportError::Stalled { .. } => {
            ErrorReason::Connection
        }
    };
    let _ = events
        .send(EngineEvent::Failed {
            turn_id: Some(turn_id),
            reason,
            message: error.to_string().into(),
        })
        .await;
}
