//! UI-independent agent runtime shared by terminal, desktop, and headless surfaces.

pub mod agents;
pub mod claims;
pub mod compaction;
pub mod findings;
pub mod goal;
pub mod protocol;
pub mod registry;
pub mod review;
pub mod runtime;
pub mod steering;
pub mod tool_agent;
pub mod tool_loop;

pub use agents::{AgentContext, AgentRequest, AgentRunner, AgentSupervisor, StreamingAgentRunner};
pub use claims::{ClaimError, Claims};
pub use findings::{FINDINGS_CAPACITY, Finding, Findings};
pub use goal::{
    CodeRequest, Coder, DEFAULT_GOAL_ROUNDS, GoalCancel, GoalLoop, GoalOutcome, GoalStop, Patch,
};
pub use protocol::{AgentKind, AgentStatus, EngineCommand, EngineEvent, TurnId};
pub use registry::{
    CredentialSource, EnvCredentialSource, HttpTransportFactory, LayeredCredentialSource,
    ModelDescriptor, ProviderDescriptor, ProviderRegistry, ProviderRegistryConfig, RegistryError,
    ResolvedModel, TransportFactory,
};
pub use review::{AgentReviewer, REVIEWER_BRIEF, Review, ReviewRequest, Reviewer, Verdict};
pub use runtime::{Engine, EngineConfig, EngineError, EngineRuntime, TransportResolver};
pub use steering::{STEERING_CAPACITY, Steering};
pub use tool_agent::{DEFAULT_AGENT_ROUNDS, ToolAgentRunner};
pub use tool_loop::{TOUCHED_CAPACITY, TOUCHING_TOOLS, TouchedSet, TouchedSink, TrajectorySink};

/// Crate version, mirrors the workspace release.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
