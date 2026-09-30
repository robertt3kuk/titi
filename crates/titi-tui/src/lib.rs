//! Terminal diff renderer: history batches, viewport diffing, overlays
//! (contract: `omp://tui-core-renderer`).

pub mod caps;
/// Crate version, mirrors the workspace release.
pub mod component;
pub mod composer;
pub mod cursor;
pub mod diff;
pub mod focus;
pub mod history;
pub mod hub;
pub mod image;
pub mod input;
pub mod keybindings;
pub mod keys;
pub mod markdown;
pub mod overlay;
pub mod panels;
pub mod recap;
pub mod renderer;
pub mod selection;
pub mod slash;
pub mod space_hold;
pub mod status;
pub mod status_bar;
pub mod theme;
pub mod transcript;
pub mod viewport;
pub mod width;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
