//! Terminal rendering kit: theme, width, panels, markdown, diff, image
//! (widgets the chat's own frame loop composes).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod caps;
/// Crate version, mirrors the workspace release.
pub mod component;
pub mod diff;
pub mod emoji;
pub mod image;
pub mod keybindings;
pub mod keys;
pub mod latex;
pub mod markdown;
pub(crate) mod mermaid;
pub mod panels;
pub mod recap;
pub mod scrollbar;
pub mod selection;
pub mod space_hold;
pub mod status;
pub mod status_bar;
pub mod theme;
pub mod width;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
