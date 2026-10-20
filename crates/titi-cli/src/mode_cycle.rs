//! Shift+Tab: the session mode, one step round agent → plan → duck → agent.
//!
//! The step is the slash command a person would have typed (`/plan`, `/duck`,
//! `/done`), so the engine's own `SetMode` is what withholds the tools, and
//! the mode the screen shows — the masthead word and the composer's badge — is
//! the engine's answer, never the keystroke.

use titi_engine::protocol::SessionMode;

use crate::chat::*;

/// The command that moves `mode` one step round the cycle.
pub(crate) fn next_mode_command(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::Agent => "/plan",
        SessionMode::Plan => "/duck",
        SessionMode::Duck => "/done",
    }
}

impl Chat {
    pub(crate) fn cycle_mode(&mut self) -> Applied {
        self.disarm();
        self.slash(next_mode_command(self.mode))
            .unwrap_or_else(Applied::none)
    }
}
