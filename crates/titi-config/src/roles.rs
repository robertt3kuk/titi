//! Role name → model id.
//!
//! `modelRoles` is an optional map. A missing map keeps the current model.
//! A role that is not in a present map is an error. This does not name
//! providers or endpoints.

use serde_json::Value;

/// Why a role did not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleError {
    /// `modelRoles` is present, and this role is not a key in it.
    Unknown(String),
    /// The map or the role value is not the shape this resolver reads.
    Invalid(String),
}

impl std::fmt::Display for RoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoleError::Unknown(role) => write!(f, "unknown model role {role}"),
            RoleError::Invalid(reason) => write!(f, "model role: {reason}"),
        }
    }
}

impl std::error::Error for RoleError {}

/// Resolve `role` against an optional `modelRoles` object.
///
/// `None` means the map is absent: return `current`. A present map that does
/// not contain `role` is [`RoleError::Unknown`].
pub fn resolve_role(map: Option<&Value>, role: &str, current: &str) -> Result<String, RoleError> {
    if map.is_none() {
        return Ok(current.to_owned());
    }
    match role_slot(map, role)? {
        None => Err(RoleError::Unknown(role.to_owned())),
        Some(RoleSlot {
            model: Some(model), ..
        }) => Ok(model),
        Some(_) => Err(RoleError::Invalid(format!("role {role} names no model"))),
    }
}

/// What one `modelRoles` entry says: a plain model id, or a map
/// `{model, effort}` where either key may be left out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleSlot {
    pub model: Option<String>,
    /// The word as written; the caller knows what levels exist.
    pub effort: Option<String>,
}

/// The slot for `role`, `None` when the map or the role is absent.
pub fn role_slot(map: Option<&Value>, role: &str) -> Result<Option<RoleSlot>, RoleError> {
    let Some(map) = map else {
        return Ok(None);
    };
    let Some(object) = map.as_object() else {
        return Err(RoleError::Invalid(
            "modelRoles must be a map of role to model id".into(),
        ));
    };
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    match object.get(role) {
        None => Ok(None),
        Some(Value::String(id)) if !id.trim().is_empty() => Ok(Some(RoleSlot {
            model: Some(id.trim().to_owned()),
            effort: None,
        })),
        Some(Value::Object(slot)) => {
            let model = text(slot.get("model"));
            let effort = text(slot.get("effort"));
            if model.is_none() && effort.is_none() {
                return Err(RoleError::Invalid(format!(
                    "role {role} needs a model or an effort"
                )));
            }
            Ok(Some(RoleSlot { model, effort }))
        }
        Some(_) => Err(RoleError::Invalid(format!(
            "role {role} must be a model id or a map with model and effort"
        ))),
    }
}

/// Read `modelRoles` from layered settings. A missing key is a missing map.
pub fn resolve_model_role(
    settings: &crate::settings::Settings,
    role: &str,
    current: &str,
) -> Result<String, RoleError> {
    resolve_role(settings.get("modelRoles").as_ref(), role, current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_missing_map_uses_the_current_model() {
        assert_eq!(
            resolve_role(None, "smol", "current-model").unwrap(),
            "current-model"
        );
        assert_eq!(
            resolve_role(None, "default", "current-model").unwrap(),
            "current-model"
        );
    }

    #[test]
    fn a_known_role_returns_its_model_id() {
        let map = json!({
            "smol": "small-id",
            "default": "mid-id",
            "slow": "large-id"
        });
        assert_eq!(
            resolve_role(Some(&map), "smol", "current-model").unwrap(),
            "small-id"
        );
        assert_eq!(
            resolve_role(Some(&map), "slow", "current-model").unwrap(),
            "large-id"
        );
    }

    #[test]
    fn an_unknown_role_is_an_error() {
        let map = json!({ "default": "mid-id" });
        let error = resolve_role(Some(&map), "vision", "current-model").unwrap_err();
        assert_eq!(error, RoleError::Unknown("vision".into()));
    }

    #[test]
    fn settings_without_the_key_fall_back() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let settings = crate::settings::Settings::load(&agent, tmp.path(), &[]).unwrap();
        assert_eq!(
            resolve_model_role(&settings, "default", "current-model").unwrap(),
            "current-model"
        );
    }

    #[test]
    fn settings_map_resolves_and_rejects_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("config.yml"),
            "modelRoles:\n  smol: small-id\n  default: mid-id\n",
        )
        .unwrap();
        let settings = crate::settings::Settings::load(&agent, tmp.path(), &[]).unwrap();
        assert_eq!(
            resolve_model_role(&settings, "smol", "current-model").unwrap(),
            "small-id"
        );
        assert!(matches!(
            resolve_model_role(&settings, "slow", "current-model"),
            Err(RoleError::Unknown(_))
        ));
    }

    #[test]
    fn a_slot_is_a_model_id_or_a_map_with_a_model_and_an_effort() {
        let map = json!({
            "plain": "a-id",
            "both": { "model": "b-id", "effort": "low" },
            "effort_only": { "effort": "high" },
            "model_only": { "model": "c-id" },
            "empty": {},
            "blank": "  ",
            "number": 7
        });
        let slot = |role| role_slot(Some(&map), role);
        assert_eq!(
            slot("plain").unwrap(),
            Some(RoleSlot {
                model: Some("a-id".into()),
                effort: None
            })
        );
        assert_eq!(
            slot("both").unwrap(),
            Some(RoleSlot {
                model: Some("b-id".into()),
                effort: Some("low".into())
            })
        );
        assert_eq!(
            slot("effort_only").unwrap(),
            Some(RoleSlot {
                model: None,
                effort: Some("high".into())
            })
        );
        assert_eq!(slot("model_only").unwrap().unwrap().effort, None);
        assert_eq!(slot("missing").unwrap(), None);
        for bad in ["empty", "blank", "number"] {
            assert!(matches!(slot(bad), Err(RoleError::Invalid(_))), "{bad}");
        }
        assert_eq!(role_slot(None, "plain").unwrap(), None);
    }

    /// `/switch @role` and every other reader of a role's model take the
    /// nested form too; a slot that names only an effort has no model to give.
    #[test]
    fn resolve_role_reads_the_model_out_of_a_nested_slot() {
        let map = json!({
            "review": { "model": "r-id", "effort": "low" },
            "think": { "effort": "high" }
        });
        assert_eq!(resolve_role(Some(&map), "review", "cur").unwrap(), "r-id");
        assert!(matches!(
            resolve_role(Some(&map), "think", "cur"),
            Err(RoleError::Invalid(_))
        ));
        assert!(matches!(
            resolve_role(Some(&map), "other", "cur"),
            Err(RoleError::Unknown(_))
        ));
    }
}
