//! The `/mcp` panel: the session's MCP servers and where each stands.
//!
//! The engine reports every change (`EngineEvent::McpStatus`) and the panel
//! draws the latest, so a server that comes up while it is open appears
//! without a key press. A row is a server with its state (`starting`,
//! `ready`, `failed` with the reason, `disabled`) and tool count; Enter opens
//! the server's tools, command, file and log; `r` restarts the selected one,
//! as `/mcp restart <name>` does. Servers a repository defines that the user
//! declined at start-up are listed as `untrusted`: they never ran, and
//! nothing here can start them — the question comes back the next time titi
//! starts in that repository.

use std::cell::Cell;
use std::time::Instant;

use titi_engine::EngineCommand;
use titi_mcp::{ServerState, ServerStatus};

use crate::chat::{Applied, Chat, Key};
use crate::composer::{one_line, wrap_plain};
use crate::hooks_setup::sanitize;
use crate::pickers::{PanelLine, PanelView, panel_body, panel_label_room, panel_view};
use crate::transcript::LineKind;

/// What the screen holds for MCP.
#[derive(Debug, Default)]
pub(crate) struct McpState {
    /// The engine's latest report, by name.
    pub(crate) servers: Vec<ServerStatus>,
    /// Project servers the user said `no` to at start-up: name, command line,
    /// file.
    pub(crate) untrusted: Vec<(String, String, String)>,
    pub(crate) picker: Option<McpPicker>,
}

#[derive(Debug, Clone)]
pub(crate) struct McpPicker {
    /// The row the cursor is on: servers first, then untrusted ones.
    selected: usize,
    /// Whether Enter has opened the selected server's details.
    detail: bool,
    scroll: usize,
    /// Lines the open details wrapped to when last drawn, so the keys can stop
    /// at their end.
    drawn_lines: Cell<usize>,
}

impl McpState {
    fn rows(&self) -> usize {
        self.servers.len() + self.untrusted.len()
    }

    fn known(&self, name: &str) -> bool {
        self.servers.iter().any(|server| server.name == name)
    }
}

impl Chat {
    /// `/mcp` opens the panel; `/mcp restart <name>` restarts one server.
    pub(crate) fn mcp_command(&mut self, args: &str) -> Applied {
        let (word, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        match word {
            "" | "list" if rest.trim().is_empty() => {
                if self.mcp.rows() == 0 {
                    self.push(
                        LineKind::Note,
                        format!(
                            "mcp: no servers — add an mcpServers map to {}/mcp.json",
                            self.agent_dir.display()
                        ),
                    );
                    return Applied::none();
                }
                self.mcp.picker = Some(McpPicker {
                    selected: 0,
                    detail: false,
                    scroll: 0,
                    drawn_lines: Cell::new(0),
                });
                Applied::none()
            }
            "restart" => self.mcp_restart(rest.trim()),
            _ => {
                self.push(
                    LineKind::Error,
                    "usage: /mcp · /mcp restart <name>".to_owned(),
                );
                Applied::none()
            }
        }
    }

    fn mcp_restart(&mut self, name: &str) -> Applied {
        if name.is_empty() {
            self.push(LineKind::Error, "usage: /mcp restart <name>".to_owned());
            return Applied::none();
        }
        if self.mcp.known(name) {
            self.push(
                LineKind::Note,
                format!("mcp: restarting {}", sanitize(name)),
            );
            return Applied::send(EngineCommand::RestartMcp { name: name.into() }, None);
        }
        let message = if self.mcp.untrusted.iter().any(|(known, ..)| known == name) {
            format!(
                "mcp: {} is untrusted and was never started; restart titi here to review it",
                sanitize(name)
            )
        } else {
            let known: Vec<&str> = self.mcp.servers.iter().map(|s| s.name.as_str()).collect();
            format!(
                "mcp: no server named {} (have: {})",
                sanitize(name),
                if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                }
            )
        };
        self.push(LineKind::Error, message);
        Applied::none()
    }

    /// The engine's report: kept, and the cursor kept inside it.
    pub(crate) fn set_mcp_status(&mut self, servers: Vec<ServerStatus>) {
        self.mcp.servers = servers;
        let rows = self.mcp.rows();
        if let Some(picker) = self.mcp.picker.as_mut() {
            picker.selected = picker.selected.min(rows.saturating_sub(1));
        }
    }

    /// A key while the panel is up: arrows move, Enter opens or closes the
    /// details, `r` restarts the selected server, Esc closes. Any other key
    /// closes the panel and goes on to do what it does.
    pub(crate) fn mcp_key(&mut self, key: Key, now: Instant) -> Applied {
        match key {
            Key::Up => self.move_mcp_cursor(-1),
            Key::Down => self.move_mcp_cursor(1),
            Key::Enter => {
                if let Some(picker) = self.mcp.picker.as_mut() {
                    picker.detail = !picker.detail;
                    picker.scroll = 0;
                }
            }
            Key::Char('r') => {
                let name = self.mcp.picker.as_ref().and_then(|picker| {
                    self.mcp
                        .servers
                        .get(picker.selected)
                        .map(|server| server.name.to_string())
                });
                return match name {
                    Some(name) => self.mcp_restart(&name),
                    None => Applied::none(),
                };
            }
            Key::Esc => {
                self.mcp.picker = None;
                self.disarm();
            }
            other => {
                self.mcp.picker = None;
                return self.on_key(other, now);
            }
        }
        Applied::none()
    }

    fn move_mcp_cursor(&mut self, delta: isize) {
        let rows = self.mcp.rows();
        let Some(picker) = self.mcp.picker.as_mut() else {
            return;
        };
        if picker.detail {
            let last = picker.drawn_lines.get().saturating_sub(1);
            picker.scroll = picker.scroll.saturating_add_signed(delta).min(last);
        } else if rows > 0 {
            picker.selected = (picker.selected as isize + delta).rem_euclid(rows as isize) as usize;
        }
    }
}

fn tools_word(count: usize) -> String {
    format!("{count} {}", if count == 1 { "tool" } else { "tools" })
}

/// The `/mcp` panel: a row per server, or the details of the one Enter
/// opened.
pub(crate) fn mcp_panel(chat: &Chat, total: u16, width: u16) -> PanelView {
    let Some(picker) = chat.mcp.picker.as_ref() else {
        return panel_view(None, Vec::new(), None, panel_body(total));
    };
    let rows = chat.mcp.rows();
    let selected = picker.selected.min(rows.saturating_sub(1));
    let room = panel_label_room(width).saturating_sub(3);
    if picker.detail
        && let Some(lines) = detail_lines(chat, selected, room)
    {
        picker.drawn_lines.set(lines.len());
        let cursor = picker.scroll.min(lines.len().saturating_sub(1));
        let title = match chat.mcp.servers.get(selected) {
            Some(server) => format!(
                "mcp · {} · {}",
                sanitize(&server.name),
                tools_word(server.tools.len())
            ),
            None => "mcp · untrusted".to_owned(),
        };
        let lines = lines
            .into_iter()
            .map(|text| PanelLine::Row {
                text,
                accent: false,
            })
            .collect();
        return panel_view(Some(title), lines, Some(cursor), panel_body(total));
    }
    let name_width = chat
        .mcp
        .servers
        .iter()
        .map(|server| server.name.chars().count())
        .chain(
            chat.mcp
                .untrusted
                .iter()
                .map(|(name, ..)| name.chars().count()),
        )
        .max()
        .unwrap_or(0)
        .min(24);
    let mut lines: Vec<PanelLine> = Vec::new();
    for server in &chat.mcp.servers {
        let what = match server.state {
            ServerState::Ready => tools_word(server.tools.len()),
            ServerState::Failed => server.reason.clone(),
            ServerState::Starting if !server.reason.is_empty() => server.reason.clone(),
            _ => String::new(),
        };
        lines.push(PanelLine::Row {
            text: one_line(
                &format!(
                    "{:<name_width$}  {:<9} {}",
                    sanitize(&server.name),
                    server.state.as_str(),
                    sanitize(&what)
                ),
                room,
            ),
            accent: false,
        });
    }
    for (name, ..) in &chat.mcp.untrusted {
        lines.push(PanelLine::Row {
            text: one_line(
                &format!(
                    "{:<name_width$}  {:<9} not started — you declined it at start-up",
                    sanitize(name),
                    "untrusted"
                ),
                room,
            ),
            accent: false,
        });
    }
    panel_view(
        Some(format!(
            "mcp · {} · Enter tools · r restart · Esc",
            if rows == 1 {
                "1 server".to_owned()
            } else {
                format!("{rows} servers")
            }
        )),
        lines,
        Some(selected),
        panel_body(total),
    )
}

/// What Enter shows for a row.
fn detail_lines(chat: &Chat, selected: usize, room: usize) -> Option<Vec<String>> {
    let mut lines = Vec::new();
    let wrapped = |text: String, indent: &str, lines: &mut Vec<String>| {
        for (row, text) in wrap_plain(&text, room.saturating_sub(indent.len()).max(8))
            .into_iter()
            .enumerate()
        {
            lines.push(if row == 0 {
                text
            } else {
                format!("{indent}{text}")
            });
        }
    };
    if let Some(server) = chat.mcp.servers.get(selected) {
        wrapped(
            format!("{} · {}", server.state.as_str(), sanitize(&server.reason)),
            "  ",
            &mut lines,
        );
        wrapped(format!("$ {}", sanitize(&server.command)), "  ", &mut lines);
        if !server.source.is_empty() {
            wrapped(
                format!("from {}", sanitize(&server.source)),
                "  ",
                &mut lines,
            );
        }
        wrapped(
            format!(
                "log {}",
                titi_mcp::log_path(&chat.agent_dir, &server.name).display()
            ),
            "  ",
            &mut lines,
        );
        for tool in &server.tools {
            let description = sanitize(tool.description.lines().next().unwrap_or(""));
            wrapped(
                if description.is_empty() {
                    sanitize(&tool.name)
                } else {
                    format!("{} — {description}", sanitize(&tool.name))
                },
                "    ",
                &mut lines,
            );
        }
        if server.state == ServerState::Ready && server.tools.is_empty() {
            lines.push("no tools".to_owned());
        }
        return Some(lines);
    }
    let (name, command, source) = chat.mcp.untrusted.get(selected - chat.mcp.servers.len())?;
    wrapped(format!("untrusted · {}", sanitize(name)), "  ", &mut lines);
    wrapped(format!("$ {}", sanitize(command)), "  ", &mut lines);
    wrapped(format!("from {}", sanitize(source)), "  ", &mut lines);
    lines.push("not started: titi asks again when it next starts here".to_owned());
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatEffect;
    use crate::pickers::panel_view_for;
    use std::sync::Arc;
    use titi_engine::EngineEvent;
    use titi_mcp::ToolSummary;
    use titi_tui::theme::Theme;

    fn theme() -> Arc<Theme> {
        let options = titi_tui::theme::loader::CreateThemeOptions {
            mode: Some(titi_tui::theme::ColorMode::Truecolor),
            ..Default::default()
        };
        match titi_tui::theme::loader::load_theme("titanium", &options) {
            Ok(theme) => Arc::new(theme),
            Err(reason) => panic!("built-in theme: {reason}"),
        }
    }

    fn chat() -> Chat {
        let mut chat = Chat::new("openai/gpt-4.1", "session-123", theme());
        chat.set_agent_dir(std::path::Path::new("/agent"));
        chat
    }

    fn server(name: &str, state: ServerState, reason: &str, tools: &[&str]) -> ServerStatus {
        ServerStatus {
            name: name.into(),
            state,
            reason: reason.to_owned(),
            tools: tools
                .iter()
                .map(|tool| ToolSummary {
                    name: format!("mcp__{name}__{tool}").into(),
                    description: format!("does {tool}\nand more"),
                })
                .collect(),
            command: "npx -y srv".to_owned(),
            source: "mcp.json".to_owned(),
        }
    }

    fn report(chat: &mut Chat) {
        chat.on_event(EngineEvent::McpStatus {
            servers: vec![
                server("files", ServerState::Ready, "", &["read", "write"]),
                server(
                    "web",
                    ServerState::Failed,
                    "the server exited (exit status: 1)",
                    &[],
                ),
                server("slow", ServerState::Starting, "", &[]),
            ],
        });
    }

    fn draw(chat: &Chat) -> Vec<String> {
        let view = panel_view_for(chat, 40, 100).expect("the panel is up");
        let mut out = vec![view.title.clone().unwrap_or_default()];
        out.extend(
            view.lines[view.window.start..view.window.start + view.window.count]
                .iter()
                .map(|line| match line {
                    PanelLine::Row { text, .. } | PanelLine::Heading(text) => text.clone(),
                }),
        );
        out
    }

    fn press(chat: &mut Chat, key: Key) -> Applied {
        chat.on_key(key, Instant::now())
    }

    fn last_note(chat: &Chat) -> String {
        chat.transcript()
            .last()
            .map(|line| line.text.clone())
            .unwrap_or_default()
    }

    #[test]
    fn with_no_servers_the_command_says_where_to_add_one() {
        let mut chat = chat();
        chat.slash("/mcp");
        assert!(!chat.panel_open());
        assert!(
            last_note(&chat).contains("/agent/mcp.json"),
            "{}",
            last_note(&chat)
        );
    }

    #[test]
    fn the_panel_lists_each_server_with_its_state_and_tool_count() {
        let mut chat = chat();
        report(&mut chat);
        chat.slash("/mcp");
        assert!(chat.panel_open());
        let frame = draw(&chat).join("\n");
        assert!(frame.contains("mcp · 3 servers"), "{frame}");
        assert!(
            frame.contains("files") && frame.contains("ready") && frame.contains("2 tools"),
            "{frame}"
        );
        assert!(
            frame.contains("failed") && frame.contains("exit status: 1"),
            "{frame}"
        );
        assert!(frame.contains("starting"), "{frame}");
    }

    #[test]
    fn enter_lists_the_selected_servers_tools_and_esc_closes() {
        let mut chat = chat();
        report(&mut chat);
        chat.slash("/mcp");
        press(&mut chat, Key::Enter);
        let frame = draw(&chat).join("\n");
        assert!(frame.contains("mcp · files · 2 tools"), "{frame}");
        assert!(frame.contains("mcp__files__read — does read"), "{frame}");
        assert!(
            !frame.contains("and more"),
            "only the first line of a description: {frame}"
        );
        assert!(frame.contains("$ npx -y srv"), "{frame}");
        assert!(frame.contains("/agent/mcp/files.log"), "{frame}");
        press(&mut chat, Key::Enter);
        assert!(draw(&chat).join("\n").contains("mcp · 3 servers"));
        press(&mut chat, Key::Esc);
        assert!(!chat.panel_open());
    }

    #[test]
    fn a_report_that_arrives_while_the_panel_is_open_redraws_it() {
        let mut chat = chat();
        report(&mut chat);
        chat.slash("/mcp");
        chat.on_event(EngineEvent::McpStatus {
            servers: vec![server("slow", ServerState::Ready, "", &["go"])],
        });
        let frame = draw(&chat).join("\n");
        assert!(frame.contains("mcp · 1 server"), "{frame}");
        assert!(
            frame.contains("slow") && frame.contains("1 tool"),
            "{frame}"
        );
        assert!(!frame.contains("files"), "{frame}");
    }

    #[test]
    fn restart_by_command_and_by_key_goes_to_the_engine() {
        let mut chat = chat();
        report(&mut chat);
        let applied = chat.slash("/mcp restart web").unwrap();
        assert!(matches!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::RestartMcp { ref name })) if name == "web"
        ));
        chat.slash("/mcp");
        press(&mut chat, Key::Down); // the second server: web
        let applied = press(&mut chat, Key::Char('r'));
        assert!(matches!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::RestartMcp { ref name })) if name == "web"
        ));
        assert!(chat.panel_open(), "restarting leaves the panel up");
    }

    #[test]
    fn restart_of_an_unknown_or_untrusted_server_is_refused_with_the_names() {
        let mut chat = chat();
        report(&mut chat);
        chat.mcp.untrusted = vec![("theirs".into(), "./srv".into(), ".mcp.json".into())];
        for (text, want) in [
            (
                "/mcp restart nope",
                "no server named nope (have: files, web, slow)",
            ),
            ("/mcp restart theirs", "untrusted and was never started"),
            ("/mcp restart", "usage: /mcp restart <name>"),
            ("/mcp bogus", "usage: /mcp"),
        ] {
            let applied = chat.slash(text).unwrap();
            assert!(applied.effect.is_none(), "{text}");
            assert!(
                last_note(&chat).contains(want),
                "{text}: {}",
                last_note(&chat)
            );
        }
    }

    #[test]
    fn a_declined_project_server_is_listed_as_untrusted_and_cannot_be_restarted() {
        let mut chat = chat();
        chat.mcp.untrusted = vec![("theirs".into(), "./srv --x".into(), ".mcp.json".into())];
        chat.slash("/mcp");
        let frame = draw(&chat).join("\n");
        assert!(
            frame.contains("theirs") && frame.contains("untrusted"),
            "{frame}"
        );
        press(&mut chat, Key::Enter);
        let frame = draw(&chat).join("\n");
        assert!(
            frame.contains("$ ./srv --x") && frame.contains(".mcp.json"),
            "{frame}"
        );
        assert!(press(&mut chat, Key::Char('r')).effect.is_none());
    }

    #[test]
    fn what_a_server_reports_is_stripped_of_control_characters() {
        let mut chat = chat();
        chat.on_event(EngineEvent::McpStatus {
            servers: vec![server(
                "evil",
                ServerState::Failed,
                "\u{1b}]52;c;x\u{7} boom",
                &[],
            )],
        });
        chat.slash("/mcp");
        let frame = draw(&chat).join("\n");
        assert!(
            !frame.contains('\u{1b}') && !frame.contains('\u{7}'),
            "{frame:?}"
        );
        assert!(frame.contains("boom"));
    }
}
