//! MCP client over stdio and Streamable HTTP: servers the user configures, their tools in the
//! registry as `mcp__<server>__<tool>`.
//!
//! Design: `docs/research/extensibility-marketplace/design-2026-10.md` (L4a, L4b).
//! Protocol: MCP 2025-11-25, see [`client`].

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod client;
pub mod http;
pub mod manager;
mod tool;
mod vet;

pub use client::{
    CallOutput, Client, ClientEvent, Launch, MAX_LINE_BYTES, MAX_RESULT_CHARS, McpError,
    STDERR_LOG_CAP, ToolInfo, render_content,
};
pub use http::HttpLaunch;
pub use manager::{
    EnvLookup, MAX_RESTARTS, ManagerConfig, McpEvent, McpManager, ServerState, ServerStatus,
    ToolSummary, log_path,
};
pub use tool::{MAX_TOOL_NAME, tool_name};
pub use vet::{MAX_DESCRIPTION, MAX_SCHEMA_BYTES, MAX_TOOLS};
