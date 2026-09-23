//! Agent runtime: sessions, agent loop, memory, trajectory capture.

pub mod compaction;
pub mod context_files;
pub mod hub;
pub mod prewalk;
pub mod session;
pub mod share;
pub mod trajectory;

pub use hub::{HubBroker, HubClient, HubError, HubEvent, HubRequest, SOCKET_NAME};
pub use session::{SessionMeta, SessionStore};
pub use share::{ShareError, ShareKey, SharePackage, open_package, seal_export, share_session};
pub use trajectory::{EventKind, TrajectoryRecorder};

/// Crate version, mirrors the workspace release.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
