//! Model routing by role: which model, and how hard it thinks, for each kind
//! of work that is not the chat turn itself.
//!
//! The slots live in the user's `modelRoles` map, next to the names the
//! model picker already uses. An entry is a plain model id (as before) or a
//! map `{model, effort}` where either key may be left out:
//!
//! ```yaml
//! modelRoles:
//!   default: anthropic/claude-sonnet-5-5
//!   explore: {model: anthropic/claude-haiku-5-5, effort: low}
//!   review: openai/gpt-5.5
//!   subagent: {effort: medium}
//! ```
//!
//! | role | work | falls back to |
//! |---|---|---|
//! | `subagent` | every subagent | the model subagents already ran on |
//! | `explore` | a subagent whose tools can only read | `subagent` |
//! | `review` | `/review` and the `/goal` reviewer | the chat model |
//! | `advisor` | `/advisor` | the chat model |
//! | `compact` | the summary a fold is written by | the chat model |
//! | `namer` | session titles | `smol`, then the chat model when no map exists |
//!
//! Model and effort are looked up along that chain independently, so an
//! `explore` entry that only sets an effort still takes its model from
//! `subagent`. With nothing set a role resolves to exactly what it did before
//! this module existed, and nothing is said.
//!
//! A slot that names a model the session cannot use — unknown to the
//! registry, or with no key — is not an error: the work runs on the chat model
//! and one notice says which slot and why. The same notice is not repeated for
//! the rest of the session.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use smol_str::SmolStr;
use titi_config::roles::{RoleSlot, role_slot};
use titi_providers::{Effort, Reasoning, RequestOpts, resolve_reasoning};

use crate::registry::ResolvedModel;
use crate::runtime::TransportResolver;

/// A kind of work with its own slot in `modelRoles`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Subagent,
    Explore,
    Review,
    Advisor,
    Compact,
    Namer,
}

impl Role {
    pub const ALL: [Role; 6] = [
        Role::Subagent,
        Role::Explore,
        Role::Review,
        Role::Advisor,
        Role::Compact,
        Role::Namer,
    ];

    /// The key in `modelRoles`.
    pub const fn name(self) -> &'static str {
        match self {
            Role::Subagent => "subagent",
            Role::Explore => "explore",
            Role::Review => "review",
            Role::Advisor => "advisor",
            Role::Compact => "compact",
            Role::Namer => "namer",
        }
    }

    /// The keys read for this role, first one first. `smol` is the name the
    /// session namer was configured under before `namer` existed.
    const fn chain(self) -> &'static [&'static str] {
        match self {
            Role::Subagent => &["subagent"],
            Role::Explore => &["explore", "subagent"],
            Role::Review => &["review"],
            Role::Advisor => &["advisor"],
            Role::Compact => &["compact"],
            Role::Namer => &["namer", "smol"],
        }
    }

    pub const fn about(self) -> &'static str {
        match self {
            Role::Subagent => "every subagent",
            Role::Explore => "a subagent that can only read",
            Role::Review => "/review and the /goal reviewer",
            Role::Advisor => "/advisor",
            Role::Compact => "the summary a fold is written by",
            Role::Namer => "session titles",
        }
    }
}

/// Why a slot did not decide the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fallback {
    pub slot: &'static str,
    pub asked: String,
    pub why: String,
}

/// Where a role's work runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub role: Role,
    pub model: SmolStr,
    /// The effort a slot asked for. Whether the model takes one is decided
    /// when the request is built ([`reasoning_for`]).
    pub effort: Option<Effort>,
    /// The slot that named `model`; `None` when nothing did and `model` is
    /// the default the caller passed in.
    pub source: Option<&'static str>,
    pub fallback: Option<Fallback>,
    /// Whether any slot of the chain exists at all, even one that did not
    /// resolve.
    pub configured: bool,
    /// What the person should be told: an unusable model, an effort that is
    /// not a level, a slot that is not a slot.
    pub notes: Vec<String>,
}

impl Route {
    /// `fake/second · effort low · modelRoles.review`, for the `/router` view.
    pub fn describe(&self) -> String {
        let mut text = self.model.to_string();
        if let Some(effort) = self.effort {
            text.push_str(&format!(" · effort {effort}"));
        }
        match (&self.fallback, self.source) {
            (Some(fallback), _) => text.push_str(&format!(
                " · modelRoles.{} `{}` cannot be used ({})",
                fallback.slot, fallback.asked, fallback.why
            )),
            (None, Some(slot)) => text.push_str(&format!(" · modelRoles.{slot}")),
            (None, None) => text.push_str(" · not set"),
        }
        text
    }

    /// What is wrong with the settings for this role other than a model that
    /// cannot run, which [`describe`](Self::describe) already says.
    pub fn problems(&self) -> Vec<&str> {
        self.notes
            .iter()
            .map(String::as_str)
            .filter(|note| {
                !self.fallback.as_ref().is_some_and(|fallback| {
                    note.starts_with(&format!(
                        "modelRoles.{}: `{}` cannot be used",
                        fallback.slot, fallback.asked
                    ))
                })
            })
            .collect()
    }
}

/// Resolves `role` against a `modelRoles` value. `default` is the model the
/// work ran on before roles existed; `usable` says whether an id can run.
pub fn resolve(
    map: Option<&Value>,
    role: Role,
    default: &str,
    usable: &dyn Fn(&str) -> Result<(), String>,
) -> Route {
    let mut route = Route {
        role,
        model: default.into(),
        effort: None,
        source: None,
        fallback: None,
        configured: false,
        notes: Vec::new(),
    };
    let mut asked: Option<(&'static str, String)> = None;
    let mut effort: Option<(&'static str, String)> = None;
    for slot in role.chain() {
        match role_slot(map, slot) {
            Ok(None) => {}
            Ok(Some(RoleSlot {
                model,
                effort: level,
            })) => {
                route.configured = true;
                if asked.is_none() {
                    asked = model.map(|model| (*slot, model));
                }
                if effort.is_none() {
                    effort = level.map(|level| (*slot, level));
                }
            }
            Err(error) => {
                let note = format!("modelRoles.{slot}: {error}");
                if !route.notes.contains(&note) {
                    route.notes.push(note);
                }
            }
        }
    }
    if let Some((slot, model)) = asked {
        match usable(&model) {
            Ok(()) => {
                route.model = model.as_str().into();
                route.source = Some(slot);
            }
            Err(why) => {
                route.notes.push(format!(
                    "modelRoles.{slot}: `{model}` cannot be used ({why}); using {default}"
                ));
                route.fallback = Some(Fallback {
                    slot,
                    asked: model,
                    why,
                });
            }
        }
    }
    if let Some((slot, level)) = effort {
        match Effort::from_name(&level) {
            Some(level) => route.effort = Some(level),
            None => route.notes.push(format!(
                "modelRoles.{slot}: effort `{level}` is not a level (minimal, low, medium, \
                 high, xhigh, max); ignored"
            )),
        }
    }
    route
}

/// The reasoning a request to `resolved` carries for `effort`: nothing for a
/// model that declares no ladder, and nothing when no effort was asked for.
pub fn reasoning_for(resolved: &ResolvedModel, effort: Option<Effort>) -> Option<Reasoning> {
    resolve_reasoning(
        resolved.transport.api(),
        resolved.reasoning.as_ref(),
        &RequestOpts { effort },
    )
}

/// Every role's route, in [`Role::ALL`] order: what the `/router` view shows.
pub fn table(
    map: Option<&Value>,
    default: &str,
    usable: &dyn Fn(&str) -> Result<(), String>,
) -> Vec<Route> {
    Role::ALL
        .into_iter()
        .map(|role| resolve(map, role, default, usable))
        .collect()
}

/// A [`Route`] and the notices it has not yet shown.
#[derive(Debug, Clone)]
pub struct Routed {
    pub route: Route,
    /// The route's notes that this session has not said before.
    pub notices: Vec<SmolStr>,
    /// Whether `modelRoles` exists in the settings at all.
    pub has_map: bool,
}

/// Reads the user's `modelRoles` when work starts, so editing the config
/// takes effect on the next review or the next subagent, and remembers which
/// notices it has already raised.
#[derive(Clone)]
pub struct Router {
    agent_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
    said: Arc<Mutex<HashSet<String>>>,
}

impl Router {
    pub fn new(agent_dir: Option<PathBuf>, workspace: Option<PathBuf>) -> Self {
        Self {
            agent_dir,
            workspace,
            said: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn model_roles(&self) -> Option<Value> {
        let agent_dir = self.agent_dir.as_ref()?;
        let workspace = self.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
        titi_config::settings::Settings::load(agent_dir, &workspace, &[])
            .ok()?
            .get("modelRoles")
    }

    /// Reads settings from disk: call it from a thread that may block, or use
    /// [`route_async`](Self::route_async).
    pub fn route(&self, role: Role, default: &str, resolver: &dyn TransportResolver) -> Routed {
        let map = self.model_roles();
        let usable = |id: &str| {
            resolver
                .resolve(id)
                .map(|_| ())
                .map_err(|error| error.to_string())
        };
        let route = resolve(map.as_ref(), role, default, &usable);
        let notices = {
            let mut said = self.said.lock().unwrap_or_else(PoisonError::into_inner);
            route
                .notes
                .iter()
                .filter(|note| said.insert((*note).clone()))
                .map(|note| SmolStr::from(note.as_str()))
                .collect()
        };
        Routed {
            route,
            notices,
            has_map: map.is_some(),
        }
    }

    pub async fn route_async(
        &self,
        role: Role,
        default: &str,
        resolver: &Arc<dyn TransportResolver>,
    ) -> Routed {
        let router = self.clone();
        let resolver = Arc::clone(resolver);
        let owned = default.to_owned();
        tokio::task::spawn_blocking(move || router.route(role, &owned, resolver.as_ref()))
            .await
            .unwrap_or_else(|_| Routed {
                route: resolve(None, role, default, &|_| Ok(())),
                notices: Vec::new(),
                has_map: false,
            })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn all_usable(_: &str) -> Result<(), String> {
        Ok(())
    }

    fn only(ids: &'static [&'static str]) -> impl Fn(&str) -> Result<(), String> {
        move |id| {
            if ids.contains(&id) {
                Ok(())
            } else {
                Err(format!("unknown model {id}"))
            }
        }
    }

    #[test]
    fn nothing_set_is_the_default_and_says_nothing() {
        for role in Role::ALL {
            for map in [None, Some(json!({"default": "x"}))] {
                let route = resolve(map.as_ref(), role, "chat", &all_usable);
                assert_eq!(route.model, "chat", "{role:?}");
                assert_eq!((route.effort, route.source), (None, None));
                assert!(!route.configured && route.notes.is_empty(), "{role:?}");
            }
        }
    }

    #[test]
    fn each_role_reads_its_own_slot() {
        let map = json!({
            "subagent": "m-sub", "explore": "m-exp", "review": "m-rev",
            "advisor": "m-adv", "compact": "m-com", "namer": "m-nam"
        });
        for role in Role::ALL {
            let route = resolve(Some(&map), role, "chat", &all_usable);
            let expected = format!("m-{}", &role.name()[..3]);
            assert_eq!(route.model, expected.as_str(), "{role:?}");
            assert_eq!(route.source, Some(role.name()));
        }
    }

    #[test]
    fn explore_falls_back_to_subagent_and_namer_to_smol() {
        let map = json!({"subagent": "m-sub", "smol": "m-small"});
        let explore = resolve(Some(&map), Role::Explore, "chat", &all_usable);
        assert_eq!(
            (explore.model.as_str(), explore.source),
            ("m-sub", Some("subagent"))
        );
        let namer = resolve(Some(&map), Role::Namer, "chat", &all_usable);
        assert_eq!(
            (namer.model.as_str(), namer.source),
            ("m-small", Some("smol"))
        );
        let both = json!({"namer": "m-nam", "smol": "m-small"});
        assert_eq!(
            resolve(Some(&both), Role::Namer, "chat", &all_usable).model,
            "m-nam"
        );
    }

    #[test]
    fn model_and_effort_are_looked_up_along_the_chain_separately() {
        let map = json!({
            "subagent": {"model": "m-sub", "effort": "high"},
            "explore": {"effort": "low"}
        });
        let explore = resolve(Some(&map), Role::Explore, "chat", &all_usable);
        assert_eq!(explore.model, "m-sub");
        assert_eq!(explore.effort, Some(Effort::Low));
        let plain = resolve(Some(&map), Role::Subagent, "chat", &all_usable);
        assert_eq!(plain.effort, Some(Effort::High));
    }

    #[test]
    fn an_unusable_model_falls_back_with_one_note_and_keeps_the_effort() {
        let map = json!({"review": {"model": "ghost", "effort": "low"}});
        let route = resolve(Some(&map), Role::Review, "chat", &only(&["chat"]));
        assert_eq!(route.model, "chat");
        assert_eq!(route.source, None);
        assert_eq!(route.effort, Some(Effort::Low));
        assert_eq!(route.notes.len(), 1);
        assert!(
            route.notes[0].starts_with("modelRoles.review: `ghost` cannot be used"),
            "{:?}",
            route.notes
        );
        assert!(route.describe().contains("cannot be used"));
    }

    #[test]
    fn a_bad_effort_or_a_bad_slot_is_noted_not_fatal() {
        let map = json!({"review": {"model": "m", "effort": "turbo"}, "advisor": 7});
        let review = resolve(Some(&map), Role::Review, "chat", &all_usable);
        assert_eq!((review.model.as_str(), review.effort), ("m", None));
        assert!(review.notes[0].contains("turbo"), "{:?}", review.notes);
        let advisor = resolve(Some(&map), Role::Advisor, "chat", &all_usable);
        assert_eq!(advisor.model, "chat");
        assert!(advisor.notes[0].starts_with("modelRoles.advisor:"));
    }

    #[test]
    fn the_router_reads_settings_and_says_a_note_once() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("config.yml"),
            "modelRoles:\n  review: ghost\n  advisor:\n    model: known\n    effort: high\n",
        )
        .unwrap();
        let resolver = |id: &str| {
            if id == "known" {
                Ok(ResolvedModel::without_credential(
                    id,
                    Arc::new(titi_providers::MockTransport::new(Vec::new())),
                ))
            } else {
                Err(crate::registry::RegistryError::UnknownModel(id.into()))
            }
        };
        let router = Router::new(Some(agent), Some(tmp.path().to_path_buf()));

        let first = router.route(Role::Review, "chat", &resolver);
        assert_eq!(first.route.model, "chat");
        assert_eq!(first.notices.len(), 1, "{:?}", first.notices);
        assert!(first.has_map);
        let again = router.route(Role::Review, "chat", &resolver);
        assert!(again.notices.is_empty(), "said once: {:?}", again.notices);

        let advisor = router.route(Role::Advisor, "chat", &resolver);
        assert_eq!(advisor.route.model, "known");
        assert_eq!(advisor.route.effort, Some(Effort::High));
        assert!(advisor.notices.is_empty());
    }

    #[test]
    fn no_agent_directory_means_nothing_is_set() {
        let router = Router::new(None, None);
        let routed = router.route(Role::Review, "chat", &|_: &str| {
            Err(crate::registry::RegistryError::UnknownModel("x".into()))
        });
        assert_eq!(routed.route.model, "chat");
        assert!(!routed.has_map && routed.notices.is_empty());
    }
}
