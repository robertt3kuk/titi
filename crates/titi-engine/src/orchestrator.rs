//! A graph over the work the engine already knows how to do.
//!
//! A node is one unit of work — an agent turn, a [`GoalLoop`], or a
//! [`council`](crate::council) — and an edge says where the graph goes next.
//! Which edge is taken depends on the node's verdict, so a graph encodes
//! "review, and if it does not pass, build again" without a surface having to
//! sequence the calls itself.
//!
//! Two things keep a graph from running forever or starting over:
//!
//! - **A cap per node.** Every node carries how many times it may run in one
//!   graph. A loop that keeps failing stops at the cap and the report names
//!   the node it stalled on, rather than burning turns in silence.
//! - **Resume from the failed node.** A verdict of [`Verdict::Fail`] is an
//!   answer and follows an edge like any other. A node that *could not run* —
//!   the runner errored, the council could not seat — is different: the graph
//!   stops, records that node, and [`Graph::resume`] restarts exactly it,
//!   keeping the visits and the caps already spent. Re-running the nodes
//!   before it would pay for work that already succeeded.
//!
//! Spec: `docs/PLAN.md` (P3-7).

use std::collections::HashMap;
use std::sync::Arc;

use smol_str::SmolStr;

use crate::agents::{AgentContext, AgentRequest, AgentRunner};
use crate::council::{CouncilMember, council_report, run_council};
use crate::goal::{Coder, Gates, GoalLoop, goal_report};
use crate::protocol::AgentKind;
use crate::review::{Reviewer, Verdict};

/// How many times a node may run in one graph unless it says otherwise.
/// Enough for a build/review pair to have a second try, not enough to spin.
pub const DEFAULT_NODE_RUNS: u32 = 3;

/// Why a graph is not runnable. Every case is a wiring mistake, caught once
/// at construction rather than halfway through a paid run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    #[error("a graph needs at least one node")]
    Empty,
    #[error("a node needs an id")]
    EmptyId,
    #[error("node id {0:?} is used twice")]
    DuplicateNode(SmolStr),
    #[error("node {from:?} has an edge to unknown node {to:?}")]
    UnknownTarget { from: SmolStr, to: SmolStr },
    #[error("node {0:?} is capped at zero runs, so the graph could never enter it")]
    ZeroRuns(SmolStr),
    #[error("nothing to resume: the run stopped as {0}, not on a failed node")]
    NotResumable(&'static str),
}

/// What one node does when the graph reaches it.
///
/// The three kinds are the engine's existing entry points, not new machinery:
/// an agent turn, [`GoalLoop`], and [`run_council`].
pub enum Job {
    /// One agent turn. The reply's first line names the verdict, the same
    /// contract [`crate::REVIEWER_BRIEF`] uses, so a node can judge its own
    /// result without a second model call.
    Agent {
        runner: Arc<dyn AgentRunner>,
        task: SmolStr,
    },
    /// A full coder/reviewer loop. Its verdict is the graph's verdict for
    /// this node.
    Goal {
        coder: Arc<dyn Coder>,
        reviewer: Arc<dyn Reviewer>,
        gates: Option<Arc<dyn Gates>>,
        goal: SmolStr,
    },
    /// A council answers; it does not judge. A fold that came back is
    /// [`Verdict::Pass`] — the answer is the artifact — and a council that
    /// could not seat or fold is a node failure the graph can resume.
    Council {
        members: Vec<CouncilMember>,
        synthesizer: Arc<dyn AgentRunner>,
        question: SmolStr,
    },
}

/// What a node produced. `verdict` is `None` only when the job could not run
/// at all, which is what makes the node resumable.
struct JobResult {
    verdict: Option<Verdict>,
    report: SmolStr,
    error: Option<SmolStr>,
}

impl JobResult {
    fn judged(verdict: Verdict, report: impl Into<SmolStr>) -> Self {
        Self {
            verdict: Some(verdict),
            report: report.into(),
            error: None,
        }
    }

    fn failed(error: impl Into<SmolStr>) -> Self {
        Self {
            verdict: None,
            report: SmolStr::default(),
            error: Some(error.into()),
        }
    }
}

impl Job {
    async fn run(&self, node: &str, attempt: u32) -> JobResult {
        match self {
            Job::Agent { runner, task } => {
                let request = AgentRequest {
                    id: format!("graph-{node}-{attempt}").into(),
                    name: node.into(),
                    task: task.clone(),
                    kind: AgentKind::Subagent,
                    parent_id: None,
                };
                match runner.run(request, AgentContext::detached()).await {
                    Err(error) => JobResult::failed(error),
                    // An empty reply judges nothing and is not an answer to
                    // route on; it is a turn that did not happen.
                    Ok(reply) if reply.trim().is_empty() => {
                        JobResult::failed("the agent answered with nothing")
                    }
                    Ok(reply) => {
                        let reply = reply.trim();
                        JobResult::judged(Verdict::parse(reply), reply)
                    }
                }
            }
            Job::Goal {
                coder,
                reviewer,
                gates,
                goal,
            } => {
                let mut loop_ = GoalLoop::new(Arc::clone(coder), Arc::clone(reviewer));
                if let Some(gates) = gates {
                    loop_ = loop_.with_gates(Arc::clone(gates));
                }
                let outcome = loop_.run(goal.clone()).await;
                let report = goal_report(&outcome);
                match outcome.verdict {
                    Some(verdict) => JobResult::judged(verdict, report),
                    // Cancelled, errored, or a gate that could not run: the
                    // goal never reached a judgement, so neither does the node.
                    None => JobResult::failed(outcome.error.unwrap_or_else(|| report.into())),
                }
            }
            Job::Council {
                members,
                synthesizer,
                question,
            } => {
                match run_council(members.clone(), Arc::clone(synthesizer), question.clone()).await
                {
                    Ok(report) => JobResult::judged(Verdict::Pass, council_report(&report)),
                    Err(error) => JobResult::failed(error.to_string()),
                }
            }
        }
    }
}

/// When an edge is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Whatever the node returned.
    Always,
    /// Only on this verdict.
    On(Verdict),
    /// Anything short of a pass — the usual "go round again" edge, and the
    /// one that keeps [`Verdict::Partial`] from quietly counting as success.
    NotPass,
}

impl Gate {
    pub fn allows(self, verdict: Verdict) -> bool {
        match self {
            Gate::Always => true,
            Gate::On(wanted) => wanted == verdict,
            Gate::NotPass => verdict != Verdict::Pass,
        }
    }
}

/// Where an edge leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    To(SmolStr),
    /// End the graph here, successfully.
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub gate: Gate,
    pub step: Step,
}

/// One node: what it runs, how often it may run, and where it goes next.
///
/// Edges are tried in order and the first whose gate allows the verdict wins,
/// so a specific edge is written before a catch-all.
pub struct Node {
    pub id: SmolStr,
    pub job: Job,
    pub max_runs: u32,
    pub edges: Vec<Edge>,
}

impl Node {
    pub fn new(id: impl Into<SmolStr>, job: Job) -> Self {
        Self {
            id: id.into(),
            job,
            max_runs: DEFAULT_NODE_RUNS,
            edges: Vec::new(),
        }
    }

    /// How many times this node may run in one graph, counting retries a loop
    /// sends back to it.
    pub fn with_max_runs(mut self, runs: u32) -> Self {
        self.max_runs = runs;
        self
    }

    pub fn on(mut self, gate: Gate, step: Step) -> Self {
        self.edges.push(Edge { gate, step });
        self
    }

    /// Unconditional edge to another node.
    pub fn then(self, id: impl Into<SmolStr>) -> Self {
        self.on(Gate::Always, Step::To(id.into()))
    }

    fn next(&self, verdict: Verdict) -> Option<&Step> {
        self.edges
            .iter()
            .find(|edge| edge.gate.allows(verdict))
            .map(|edge| &edge.step)
    }
}

/// Why a graph stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphStop {
    /// A [`Step::Done`] was taken, or no edge matched the last verdict.
    #[default]
    Finished,
    /// A node could not run. [`GraphRun::resume_from`] names it.
    NodeFailed,
    /// A node was entered as many times as it allowed and the graph still
    /// wanted to enter it again.
    LoopCap,
}

impl GraphStop {
    fn label(self) -> &'static str {
        match self {
            GraphStop::Finished => "finished",
            GraphStop::NodeFailed => "node failed",
            GraphStop::LoopCap => "loop cap",
        }
    }
}

/// One entry into one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeVisit {
    pub node: SmolStr,
    /// Which run of that node this was, counting from 1.
    pub attempt: u32,
    /// `None` when the job could not run.
    pub verdict: Option<Verdict>,
    /// What the job produced: the agent's reply, the goal's line, the
    /// council's block. Empty when the node failed.
    pub report: SmolStr,
    pub error: Option<SmolStr>,
}

/// What one pass over the graph did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphRun {
    pub stop: GraphStop,
    /// The last verdict any node reached. `None` when the very first node
    /// failed, since nothing was ever judged.
    pub verdict: Option<Verdict>,
    pub visits: Vec<NodeVisit>,
    /// The node the run stopped on: the one that failed, or the one that hit
    /// its cap. `None` when the graph finished.
    pub stopped_at: Option<SmolStr>,
    /// Runs already spent per node. Carried across a resume so a retry cannot
    /// buy itself a fresh cap.
    runs: HashMap<SmolStr, u32>,
}

impl GraphRun {
    /// The node [`Graph::resume`] would restart, when there is one. A run
    /// that stopped at a loop cap is not resumable: the cap would be spent
    /// again immediately, and the graph, not the node, is what needs changing.
    pub fn resume_from(&self) -> Option<&str> {
        match self.stop {
            GraphStop::NodeFailed => self.stopped_at.as_deref(),
            _ => None,
        }
    }

    /// How many times `node` has run across this graph, resumes included.
    pub fn runs(&self, node: &str) -> u32 {
        self.runs.get(node).copied().unwrap_or(0)
    }
}

/// A validated graph. Construction checks the wiring so a run never has to
/// deal with a dangling edge.
pub struct Graph {
    nodes: Vec<Node>,
    index: HashMap<SmolStr, usize>,
}

/// Nodes carry runners, so a graph prints as its wiring: the node ids in
/// order, which is what a caller debugging a route needs to see.
impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.nodes.iter().map(|node| &node.id))
            .finish()
    }
}

impl Graph {
    pub fn new(nodes: Vec<Node>) -> Result<Self, GraphError> {
        if nodes.is_empty() {
            return Err(GraphError::Empty);
        }
        let mut index = HashMap::with_capacity(nodes.len());
        for (position, node) in nodes.iter().enumerate() {
            if node.id.trim().is_empty() {
                return Err(GraphError::EmptyId);
            }
            if node.max_runs == 0 {
                return Err(GraphError::ZeroRuns(node.id.clone()));
            }
            if index.insert(node.id.clone(), position).is_some() {
                return Err(GraphError::DuplicateNode(node.id.clone()));
            }
        }
        for node in &nodes {
            for edge in &node.edges {
                if let Step::To(target) = &edge.step
                    && !index.contains_key(target)
                {
                    return Err(GraphError::UnknownTarget {
                        from: node.id.clone(),
                        to: target.clone(),
                    });
                }
            }
        }
        Ok(Self { nodes, index })
    }

    /// The node a fresh run starts from: the first one given.
    pub fn start(&self) -> &str {
        self.nodes
            .first()
            .map(|node| node.id.as_str())
            .unwrap_or_default()
    }

    /// Runs from the start node.
    pub async fn run(&self) -> GraphRun {
        let start = self.index.get(self.start()).copied().unwrap_or(0);
        self.execute(start, GraphRun::default()).await
    }

    /// Runs again from the node `previous` failed on, keeping everything that
    /// already happened: the visits stay, and the caps stay spent.
    pub async fn resume(&self, previous: &GraphRun) -> Result<GraphRun, GraphError> {
        let Some(node) = previous.resume_from() else {
            return Err(GraphError::NotResumable(previous.stop.label()));
        };
        let Some(index) = self.index.get(node).copied() else {
            return Err(GraphError::UnknownTarget {
                from: SmolStr::new_inline("resume"),
                to: node.into(),
            });
        };
        let carried = GraphRun {
            visits: previous.visits.clone(),
            verdict: previous.verdict,
            runs: previous.runs.clone(),
            ..GraphRun::default()
        };
        Ok(self.execute(index, carried).await)
    }

    async fn execute(&self, from: usize, mut run: GraphRun) -> GraphRun {
        let mut current = Some(from);
        while let Some(position) = current {
            let Some(node) = self.nodes.get(position) else {
                break;
            };
            let spent = run.runs.entry(node.id.clone()).or_insert(0);
            if *spent >= node.max_runs {
                run.stop = GraphStop::LoopCap;
                run.stopped_at = Some(node.id.clone());
                return run;
            }
            *spent += 1;
            let attempt = *spent;

            let result = node.job.run(&node.id, attempt).await;
            run.visits.push(NodeVisit {
                node: node.id.clone(),
                attempt,
                verdict: result.verdict,
                report: result.report,
                error: result.error,
            });

            let Some(verdict) = result.verdict else {
                run.stop = GraphStop::NodeFailed;
                run.stopped_at = Some(node.id.clone());
                return run;
            };
            run.verdict = Some(verdict);
            current = match node.next(verdict) {
                // No edge for this verdict is an end, not a mistake: it is how
                // a terminal node is written.
                None | Some(Step::Done) => None,
                Some(Step::To(target)) => self.index.get(target).copied(),
            };
        }
        run.stop = GraphStop::Finished;
        run.stopped_at = None;
        run
    }
}

/// Builds the graph and runs it. Mirrors [`crate::run_goal`] and
/// [`crate::run_council`]: the caller renders either outcome.
pub async fn run_graph(nodes: Vec<Node>) -> Result<GraphRun, GraphError> {
    Ok(Graph::new(nodes)?.run().await)
}

/// The transcript block a surface shows: how it ended, the final verdict, and
/// one line per node entered, in the order they ran.
pub fn graph_report(run: &GraphRun) -> String {
    let verdict = run
        .verdict
        .map(|verdict| verdict.as_str().to_ascii_lowercase())
        .unwrap_or_else(|| "no verdict".to_owned());
    let nodes = if run.visits.len() == 1 {
        "1 node".to_owned()
    } else {
        format!("{} nodes", run.visits.len())
    };
    let mut block = format!("graph: {} · {nodes} · {verdict}", run.stop.label());
    if let Some(node) = &run.stopped_at {
        block.push_str(&format!(" · at {node}"));
    }
    if run.resume_from().is_some() {
        block.push_str(" · resumable");
    }
    for (position, visit) in run.visits.iter().enumerate() {
        let outcome = match (visit.verdict, &visit.error) {
            (Some(verdict), _) => verdict.as_str().to_ascii_lowercase(),
            (None, Some(error)) => format!("failed: {error}"),
            (None, None) => "no verdict".to_owned(),
        };
        let summary = visit
            .report
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("");
        block.push_str(&format!(
            "\n{}. {}#{} · {outcome}",
            position + 1,
            visit.node,
            visit.attempt
        ));
        if !summary.is_empty() {
            block.push_str(" · ");
            block.push_str(summary);
        }
    }
    block
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// A runner that answers from a script and remembers what it was asked.
    /// A script entry of `Err` is a runner that could not run at all.
    struct ScriptedRunner {
        replies: Mutex<Vec<Result<SmolStr, SmolStr>>>,
        calls: Mutex<Vec<SmolStr>>,
    }

    impl ScriptedRunner {
        fn new(replies: Vec<Result<&str, &str>>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|reply| match reply {
                            Ok(text) => Ok(SmolStr::from(text)),
                            Err(error) => Err(SmolStr::from(error)),
                        })
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn ok(replies: Vec<&str>) -> Arc<Self> {
            Self::new(replies.into_iter().map(Ok).collect())
        }

        fn calls(&self) -> usize {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).len()
        }
    }

    #[async_trait]
    impl AgentRunner for ScriptedRunner {
        async fn run(
            &self,
            request: AgentRequest,
            _context: AgentContext,
        ) -> Result<SmolStr, SmolStr> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request.task.clone());
            let mut replies = self.replies.lock().unwrap_or_else(|e| e.into_inner());
            if replies.is_empty() {
                return Err("script exhausted".into());
            }
            replies.remove(0)
        }
    }

    fn agent(runner: Arc<ScriptedRunner>, task: &str) -> Job {
        Job::Agent {
            runner,
            task: task.into(),
        }
    }

    #[tokio::test]
    async fn a_linear_graph_runs_its_nodes_in_order() {
        let plan = ScriptedRunner::ok(vec!["PASS\nplanned"]);
        let build = ScriptedRunner::ok(vec!["PASS\nbuilt"]);
        let run = run_graph(vec![
            Node::new("plan", agent(Arc::clone(&plan), "plan it")).then("build"),
            Node::new("build", agent(Arc::clone(&build), "build it")),
        ])
        .await
        .expect("the graph is wired");

        assert_eq!(run.stop, GraphStop::Finished);
        assert_eq!(run.verdict, Some(Verdict::Pass));
        let order: Vec<&str> = run.visits.iter().map(|visit| visit.node.as_str()).collect();
        assert_eq!(order, ["plan", "build"]);
        assert_eq!(plan.calls(), 1);
        assert_eq!(build.calls(), 1);
        assert!(run.resume_from().is_none());
    }

    #[tokio::test]
    async fn a_resume_reruns_the_failed_node_and_nothing_before_it() {
        let plan = ScriptedRunner::ok(vec!["PASS\nplanned"]);
        // The build runner is down for the first call and back for the second.
        let build = ScriptedRunner::new(vec![Err("provider unreachable"), Ok("PASS\nbuilt")]);
        let ship = ScriptedRunner::ok(vec!["PASS\nshipped"]);
        let graph = Graph::new(vec![
            Node::new("plan", agent(Arc::clone(&plan), "plan it")).then("build"),
            Node::new("build", agent(Arc::clone(&build), "build it")).then("ship"),
            Node::new("ship", agent(Arc::clone(&ship), "ship it")),
        ])
        .expect("the graph is wired");

        let failed = graph.run().await;
        assert_eq!(failed.stop, GraphStop::NodeFailed);
        assert_eq!(failed.resume_from(), Some("build"));
        assert_eq!(
            failed.visits.last().and_then(|visit| visit.error.clone()),
            Some(SmolStr::from("provider unreachable"))
        );
        assert_eq!(ship.calls(), 0, "the graph stopped before ship");

        let resumed = graph.resume(&failed).await.expect("a failed node resumes");
        assert_eq!(resumed.stop, GraphStop::Finished);
        assert_eq!(resumed.verdict, Some(Verdict::Pass));
        // The node that already passed is not paid for twice.
        assert_eq!(plan.calls(), 1, "plan must not run again");
        assert_eq!(build.calls(), 2);
        assert_eq!(ship.calls(), 1);
        // The resume continues the same run rather than starting a new one.
        let order: Vec<(&str, u32)> = resumed
            .visits
            .iter()
            .map(|visit| (visit.node.as_str(), visit.attempt))
            .collect();
        assert_eq!(
            order,
            [("plan", 1), ("build", 1), ("build", 2), ("ship", 1)]
        );
        assert!(resumed.resume_from().is_none());
    }

    #[tokio::test]
    async fn a_finished_run_has_nothing_to_resume() {
        let only = ScriptedRunner::ok(vec!["PASS\ndone"]);
        let graph =
            Graph::new(vec![Node::new("only", agent(only, "do it"))]).expect("a one-node graph");
        let run = graph.run().await;
        assert!(matches!(
            graph.resume(&run).await,
            Err(GraphError::NotResumable("finished"))
        ));
    }

    #[tokio::test]
    async fn a_gate_sends_each_verdict_down_its_own_edge() {
        // Same graph, two runs: the only difference is what `check` answers.
        let build_graph = |check_reply: &str| {
            let check = ScriptedRunner::ok(vec![check_reply]);
            let fix = ScriptedRunner::ok(vec!["PASS\nfixed"]);
            let release = ScriptedRunner::ok(vec!["PASS\nreleased"]);
            (
                Graph::new(vec![
                    Node::new("check", agent(Arc::clone(&check), "check it"))
                        .on(Gate::On(Verdict::Pass), Step::To("release".into()))
                        .on(Gate::NotPass, Step::To("fix".into())),
                    Node::new("fix", agent(Arc::clone(&fix), "fix it")),
                    Node::new("release", agent(Arc::clone(&release), "release it")),
                ])
                .expect("the graph is wired"),
                fix,
                release,
            )
        };

        let (graph, fix, release) = build_graph("PASS\nall good");
        let run = graph.run().await;
        assert_eq!(
            run.visits
                .iter()
                .map(|visit| visit.node.as_str())
                .collect::<Vec<_>>(),
            ["check", "release"]
        );
        assert_eq!(fix.calls(), 0);
        assert_eq!(release.calls(), 1);

        let (graph, fix, release) = build_graph("FAIL\nbroken");
        let run = graph.run().await;
        assert_eq!(
            run.visits
                .iter()
                .map(|visit| visit.node.as_str())
                .collect::<Vec<_>>(),
            ["check", "fix"]
        );
        assert_eq!(fix.calls(), 1);
        assert_eq!(release.calls(), 0);
        assert_eq!(run.stop, GraphStop::Finished);

        // A reply that names no verdict is PARTIAL, and PARTIAL is not a pass.
        let (graph, fix, _release) = build_graph("it depends");
        let run = graph.run().await;
        assert_eq!(run.visits[0].verdict, Some(Verdict::Partial));
        assert_eq!(fix.calls(), 1);
    }

    #[tokio::test]
    async fn a_loop_stops_at_its_cap_and_names_the_node() {
        // `build` never passes and sends itself round again.
        let build = ScriptedRunner::ok(vec!["FAIL\nstill broken"; 5]);
        let graph = Graph::new(vec![
            Node::new("build", agent(Arc::clone(&build), "build it"))
                .with_max_runs(2)
                .on(Gate::On(Verdict::Pass), Step::Done)
                .on(Gate::NotPass, Step::To("build".into())),
        ])
        .expect("the graph is wired");

        let run = graph.run().await;
        assert_eq!(run.stop, GraphStop::LoopCap);
        assert_eq!(run.stopped_at.as_deref(), Some("build"));
        assert_eq!(build.calls(), 2, "the cap is a cap, not a suggestion");
        assert_eq!(run.runs("build"), 2);
        // A cap is the graph's problem, not the node's: retrying it would
        // spend the same cap again.
        assert!(run.resume_from().is_none());

        let report = graph_report(&run);
        assert!(
            report.starts_with("graph: loop cap · 2 nodes · fail · at build"),
            "{report}"
        );
        assert!(report.contains("build#2"), "{report}");
    }

    #[tokio::test]
    async fn a_dangling_edge_is_refused_before_anything_runs() {
        let runner = ScriptedRunner::ok(vec!["PASS"]);
        let error = Graph::new(vec![
            Node::new("a", agent(Arc::clone(&runner), "a")).then("nowhere"),
        ])
        .expect_err("a dangling edge is a wiring mistake");
        assert_eq!(
            error,
            GraphError::UnknownTarget {
                from: "a".into(),
                to: "nowhere".into()
            }
        );
        assert_eq!(runner.calls(), 0);

        let runner = ScriptedRunner::ok(vec!["PASS"]);
        assert_eq!(
            Graph::new(vec![
                Node::new("a", agent(Arc::clone(&runner), "a")),
                Node::new("a", agent(Arc::clone(&runner), "a")),
            ])
            .expect_err("two nodes cannot share an id"),
            GraphError::DuplicateNode("a".into())
        );
        assert!(matches!(
            Graph::new(vec![
                Node::new("a", agent(Arc::clone(&runner), "a")).with_max_runs(0)
            ]),
            Err(GraphError::ZeroRuns(_))
        ));
        assert!(matches!(
            run_graph(Vec::new()).await,
            Err(GraphError::Empty)
        ));
    }

    #[tokio::test]
    async fn an_empty_answer_is_a_failure_the_graph_can_resume() {
        let quiet = ScriptedRunner::new(vec![Ok("   "), Ok("PASS\nspoke up")]);
        let graph = Graph::new(vec![Node::new("ask", agent(Arc::clone(&quiet), "ask it"))])
            .expect("the graph is wired");

        let run = graph.run().await;
        assert_eq!(run.stop, GraphStop::NodeFailed);
        assert_eq!(run.resume_from(), Some("ask"));
        assert_eq!(run.verdict, None, "nothing was ever judged");

        let resumed = graph.resume(&run).await.expect("resumable");
        assert_eq!(resumed.stop, GraphStop::Finished);
        assert_eq!(resumed.verdict, Some(Verdict::Pass));
    }

    /// Goals and councils are nodes like any other: the goal's own verdict
    /// routes the graph, and a council that folded is a pass.
    #[tokio::test]
    async fn goal_and_council_nodes_carry_their_own_verdicts() {
        use crate::council::CouncilMember;
        use crate::goal::{CodeRequest, Patch};
        use crate::review::{Review, ReviewRequest};
        use titi_providers::Effort;

        struct OneShotCoder;
        #[async_trait]
        impl Coder for OneShotCoder {
            async fn code(&self, _request: CodeRequest) -> Result<Patch, SmolStr> {
                Ok(Patch::new("diff --git a/a b/a"))
            }
        }
        struct FailingReviewer;
        #[async_trait]
        impl Reviewer for FailingReviewer {
            async fn review(&self, _request: ReviewRequest) -> Result<Review, SmolStr> {
                Ok(Review {
                    verdict: Verdict::Fail,
                    notes: "not there yet".into(),
                })
            }
        }

        let member = |name: &str| {
            CouncilMember::new(
                name,
                "brief",
                "test/model",
                Effort::Medium,
                ScriptedRunner::ok(vec!["an opinion"]),
            )
        };
        let fold = ScriptedRunner::ok(vec!["Agreement: ship it\nDissent: none"]);
        let escalate = ScriptedRunner::ok(vec!["PASS\nescalated"]);

        let run = run_graph(vec![
            Node::new(
                "build",
                Job::Goal {
                    coder: Arc::new(OneShotCoder),
                    reviewer: Arc::new(FailingReviewer),
                    gates: None,
                    goal: "make it work".into(),
                },
            )
            .with_max_runs(1)
            .on(Gate::On(Verdict::Pass), Step::Done)
            .on(Gate::NotPass, Step::To("council".into())),
            Node::new(
                "council",
                Job::Council {
                    members: vec![member("advocate"), member("skeptic")],
                    synthesizer: fold,
                    question: "what now?".into(),
                },
            )
            .then("escalate"),
            Node::new("escalate", agent(Arc::clone(&escalate), "escalate it")),
        ])
        .await
        .expect("the graph is wired");

        assert_eq!(run.stop, GraphStop::Finished);
        let trail: Vec<(&str, Option<Verdict>)> = run
            .visits
            .iter()
            .map(|visit| (visit.node.as_str(), visit.verdict))
            .collect();
        assert_eq!(
            trail,
            [
                // The goal spent its single round without a pass.
                ("build", Some(Verdict::Fail)),
                ("council", Some(Verdict::Pass)),
                ("escalate", Some(Verdict::Pass)),
            ]
        );
        assert_eq!(escalate.calls(), 1);
    }
}
