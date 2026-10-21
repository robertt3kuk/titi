#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod actions;
pub mod changelog;
pub mod chat;
pub mod composer;
pub(crate) mod composer_state;
pub(crate) mod draft_stash;
#[cfg(test)]
mod effort_tests;
pub mod engine;
pub(crate) mod errors_panel;
#[cfg(test)]
mod errors_tests;
pub mod first_frame;
pub(crate) mod footer_actions;
#[cfg(test)]
mod footer_tests;
pub mod genome_cmd;
pub mod git_checkpoint;
pub mod headless;
pub mod herdr;
pub(crate) mod history_nav;
pub mod hub;
pub mod keys;
pub mod login;
pub(crate) mod mention;
pub(crate) mod mode_cycle;
pub mod ompcast;
pub(crate) mod palette;
pub mod pickers;
pub mod recap;
pub mod reveal;
pub mod review_cmd;
pub mod secrets;
pub mod session_fs;
pub mod session_log;
pub mod themes;
pub mod title;
pub mod trace_cmd;
pub mod transcript;
#[cfg(test)]
mod usage_tests;
pub mod vim;
pub mod wallclock;
pub mod welcome;
