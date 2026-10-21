//! `/verify`: the turn-end check, shown, detected once, and saved on request.
//!
//! `/verify` says what runs when a turn that edited files ends. With nothing
//! configured it does not guess a command to run — it runs `project check`'s
//! detection once, shows the line it found, and offers it: `/verify save`
//! writes it to the user's own config as `verify.command`, which is the user's
//! consent to run it. `/verify trust` is the same consent for a command a
//! repository's `.titi/config.yml` asks for.

use std::time::Instant;

use titi_config::settings::Settings;
use titi_config::verify::COMMAND_KEY;
use titi_engine::protocol::VerifyOutcome;

use crate::chat::{Applied, Chat, WorkPhase};
use crate::hooks_setup::sanitize;
use crate::transcript::LineKind;
use crate::verify_setup::{VerifyStartup, clip, finished_row};

/// The `call_id` the status row shows the running check under: not a tool
/// call, so nothing a `ToolFinished` could answer.
const ROW_ID: &str = "verify";

impl Chat {
    pub(crate) fn verify_started(&mut self, command: &str) {
        self.reveal_all();
        self.phase = WorkPhase::Tool {
            call_id: ROW_ID.to_owned(),
            name: "verify".to_owned(),
            detail: Some(clip(&sanitize(command))),
            since: Instant::now(),
        };
    }

    pub(crate) fn verify_finished(
        &mut self,
        command: &str,
        outcome: VerifyOutcome,
        exit_code: Option<i32>,
        ms: u64,
        retrying: bool,
    ) {
        self.phase = WorkPhase::Waiting;
        let (row, last_word) = finished_row(command, outcome, exit_code, ms, retrying);
        let kind = if last_word {
            LineKind::Error
        } else {
            LineKind::Note
        };
        self.push(kind, row);
    }

    pub(crate) fn verify_command(&mut self, args: &str) -> Applied {
        self.verify_command_in(args, &crate::session_fs::current_workspace())
    }

    pub(crate) fn verify_command_in(&mut self, args: &str, workspace: &std::path::Path) -> Applied {
        let settings = match Settings::load(&self.agent_dir, workspace, &[]) {
            Ok(settings) => settings,
            Err(reason) => {
                self.push(
                    LineKind::Error,
                    format!("verify: config load said: {reason}"),
                );
                return Applied::none();
            }
        };
        let mut start = VerifyStartup::resolve(&self.agent_dir, workspace, Some(&settings), false);
        match args {
            "" => {
                self.verify_status(&start, workspace);
                Applied::none()
            }
            "save" => self.verify_save(&start, settings, workspace),
            "trust" => match start.trust_pending() {
                Ok(Some(command)) => {
                    self.push(
                        LineKind::Note,
                        format!(
                            "verify: trusted this repository's `{}`; it runs when a turn that edited files ends",
                            clip(&sanitize(&command))
                        ),
                    );
                    Applied::send(start.engine_command(), None)
                }
                Ok(None) => {
                    self.push(
                        LineKind::Note,
                        "verify: this repository asks for no verify command".to_owned(),
                    );
                    Applied::none()
                }
                Err(reason) => {
                    self.push(LineKind::Error, format!("verify: not trusted ({reason})"));
                    Applied::none()
                }
            },
            other => {
                self.push(
                    LineKind::Error,
                    format!("verify: unknown argument `{other}`; usage: /verify [save|trust]"),
                );
                Applied::none()
            }
        }
    }

    fn verify_status(&mut self, start: &VerifyStartup, workspace: &std::path::Path) {
        let when = if start.on_edit {
            "when a turn that edited files ends"
        } else {
            "at the end of every turn (verify.onEdit is off)"
        };
        if let Some(command) = &start.command {
            let whose = if start.from_project {
                "this repository's, trusted"
            } else {
                "your config's"
            };
            self.push(
                LineKind::Note,
                format!(
                    "verify: `{}` ({whose}) runs {when}",
                    clip(&sanitize(command))
                ),
            );
        }
        if let Some(command) = &start.pending {
            self.push(
                LineKind::Note,
                format!(
                    "verify: this repository asks for `{}`, which is not trusted and does not run; /verify trust approves it",
                    clip(&sanitize(command))
                ),
            );
        }
        if start.command.is_some() || start.pending.is_some() {
            return;
        }
        match titi_tools::check_command(workspace) {
            Some(detected) => self.push(
                LineKind::Note,
                format!(
                    "verify: no {COMMAND_KEY} is set, so nothing runs when a turn ends. \
                     `project check` would run `{detected}`; /verify save keeps it as {COMMAND_KEY} in your config"
                ),
            ),
            None => self.push(
                LineKind::Note,
                format!(
                    "verify: no {COMMAND_KEY} is set and no project check was detected here; \
                     set it in your config to run one when a turn ends"
                ),
            ),
        }
    }

    fn verify_save(
        &mut self,
        start: &VerifyStartup,
        mut settings: Settings,
        workspace: &std::path::Path,
    ) -> Applied {
        if let Some(command) = start.command.as_ref().or(start.pending.as_ref()) {
            self.push(
                LineKind::Error,
                format!(
                    "verify: {COMMAND_KEY} is already `{}`; nothing saved",
                    clip(&sanitize(command))
                ),
            );
            return Applied::none();
        }
        let Some(detected) = titi_tools::check_command(workspace) else {
            self.push(
                LineKind::Error,
                "verify: no project check was detected here, so there is nothing to save"
                    .to_owned(),
            );
            return Applied::none();
        };
        if let Err(reason) = settings.set(COMMAND_KEY, serde_json::json!(detected)) {
            self.push(LineKind::Error, format!("verify: not saved ({reason})"));
            return Applied::none();
        }
        self.push(
            LineKind::Note,
            format!("verify: saved `{detected}` as {COMMAND_KEY} in your config"),
        );
        let saved = VerifyStartup::resolve(&self.agent_dir, workspace, Some(&settings), false);
        Applied::send(saved.engine_command(), None)
    }
}
