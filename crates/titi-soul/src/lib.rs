//! System prompt slots: SOUL.md identity slot #1, personality overlays, injection scan.
//!
//! Spec: docs/research/system-prompt-soul/README.md
//!
//! - [`SystemPromptBuilder::build`] assembles the identity-bearing slots:
//!   soul (slot #1), personality, appendix, plus the aggregate scan verdict.
//! - [`soul`] loads/seeds `<agent_dir>/SOUL.md` (auto-seed, never overwrite,
//!   fallback to the built-in default identity).
//! - [`personality`] resolves the personality slot: built-in presets,
//!   `PERSONALITY.md` override, session-level [`Overlay`].
//! - [`scan`] detects prompt-injection patterns before content is injected.

mod builder;
mod personality;
mod scan;
mod soul;

pub use builder::{SystemPrompt, SystemPromptBuilder};
pub use personality::{Overlay, PersonalityPreset};
pub use scan::{Pattern, ScanVerdict, scan};
pub use soul::{DEFAULT_IDENTITY, MAX_SOUL_BYTES, Soul, SoulSource};

use std::fmt;

/// Crate version, mirrors the workspace release.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Soul error: filesystem failure while reading or seeding identity files.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "soul io error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
