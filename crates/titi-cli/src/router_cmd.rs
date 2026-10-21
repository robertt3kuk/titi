//! `/router`: where each kind of side work runs, read from the config as the
//! engine would read it, changing nothing.
//!
//! The rows come from the engine's own resolution
//! ([`titi_engine::router::table`]), so what this prints is what a review, a
//! subagent or a fold will do the next time it starts. A model counts as
//! usable here when it is in the model list the picker offers; one that is
//! listed but has no key still falls back at run time, with a notice.

use std::path::Path;

use titi_engine::router::{Role, table};

use crate::chat::{Applied, Chat};
use crate::transcript::LineKind;

/// The lines `/router` prints for `agent_dir` and `workspace`, with `chat` as
/// the model every unset role runs on and `models` as the ones that can run.
pub(crate) fn router_lines(
    agent_dir: &Path,
    workspace: &Path,
    chat: &str,
    models: &[String],
) -> Vec<String> {
    let map = titi_config::settings::Settings::load(agent_dir, workspace, &[])
        .ok()
        .and_then(|settings| settings.get("modelRoles"));
    let usable = |id: &str| {
        if id == chat || models.iter().any(|model| model == id) {
            Ok(())
        } else {
            Err("not in your model list".to_owned())
        }
    };
    let mut lines = vec![format!("router · chat model {chat}")];
    for route in table(map.as_ref(), chat, &usable) {
        let name = route.role.name();
        // With a role map that names neither `namer` nor `smol` nobody names
        // the session: that is a state, not a default.
        let text = if route.role == Role::Namer && map.is_some() && !route.configured {
            "off (modelRoles names neither namer nor smol)".to_owned()
        } else {
            route.describe()
        };
        lines.push(format!("  {name:<9} {text}  — {}", route.role.about()));
        for problem in route.problems() {
            lines.push(format!("    ! {problem}"));
        }
    }
    lines.push(
        "  set one in config.yml: modelRoles: {review: {model: <id>, effort: low}}; unset roles \
         use the chat model"
            .to_owned(),
    );
    lines
}

impl Chat {
    /// `/router`: print the routing table.
    pub(crate) fn router_command(&mut self, args: &str) -> Applied {
        if !args.is_empty() {
            self.push(LineKind::Note, format!("usage: /router (got {args})"));
            return Applied::none();
        }
        let models = self.catalog.ids();
        let lines = router_lines(&self.agent_dir, &self.workspace, &self.model, &models);
        for line in lines {
            self.push(LineKind::Note, line);
        }
        Applied::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(config: &str, models: &[&str]) -> Vec<String> {
        let agent = tempfile::tempdir().unwrap();
        if !config.is_empty() {
            std::fs::write(agent.path().join("config.yml"), config).unwrap();
        }
        let models: Vec<String> = models.iter().map(|m| (*m).to_owned()).collect();
        router_lines(agent.path(), agent.path(), "fake/scripted", &models)
    }

    fn row<'a>(lines: &'a [String], role: &str) -> &'a str {
        lines
            .iter()
            .find(|line| line.trim_start().starts_with(role))
            .unwrap_or_else(|| panic!("no {role} row in {lines:#?}"))
    }

    #[test]
    fn nothing_set_shows_every_role_on_the_chat_model() {
        let out = lines("", &["fake/scripted"]);
        assert_eq!(out[0], "router · chat model fake/scripted");
        for role in Role::ALL {
            let line = row(&out, role.name());
            assert!(line.contains("fake/scripted · not set"), "{role:?}: {line}");
        }
        assert!(out.iter().all(|line| !line.contains('!')), "{out:#?}");
    }

    #[test]
    fn roles_show_their_model_effort_and_the_slot_that_decided() {
        let out = lines(
            "modelRoles:\n  subagent: fake/second\n  review: {model: fake/second, effort: low}\n  \
             explore: {effort: high}\n  smol: fake/second\n",
            &["fake/scripted", "fake/second"],
        );
        assert!(row(&out, "review").contains("fake/second · effort low · modelRoles.review"));
        // `explore` takes its model from `subagent` and its effort from itself.
        assert!(
            row(&out, "explore").contains("fake/second · effort high · modelRoles.subagent"),
            "{out:#?}"
        );
        assert!(row(&out, "namer").contains("fake/second · modelRoles.smol"));
        assert!(row(&out, "advisor").contains("fake/scripted · not set"));
    }

    #[test]
    fn an_unusable_model_and_a_bad_effort_are_shown_not_hidden() {
        let out = lines(
            "modelRoles:\n  review: ghost\n  advisor: {model: fake/second, effort: turbo}\n",
            &["fake/scripted", "fake/second"],
        );
        let review = row(&out, "review");
        assert!(review.contains("fake/scripted"), "{review}");
        assert!(review.contains("`ghost` cannot be used (not in your model list)"));
        assert!(
            out.iter()
                .any(|line| line.contains("! modelRoles.advisor: effort `turbo` is not a level")),
            "{out:#?}"
        );
        // The fallback is on the row, not repeated as a problem under it.
        assert!(
            !out.iter().any(|line| line.contains("! modelRoles.review")),
            "{out:#?}"
        );
    }

    #[test]
    fn a_role_map_without_namer_or_smol_says_the_namer_is_off() {
        let out = lines(
            "modelRoles:\n  default: fake/scripted\n",
            &["fake/scripted"],
        );
        assert!(row(&out, "namer").contains("off (modelRoles names neither namer nor smol)"));
    }
}
