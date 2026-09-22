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
    let Some(map) = map else {
        return Ok(current.to_owned());
    };
    let Some(object) = map.as_object() else {
        return Err(RoleError::Invalid(
            "modelRoles must be a map of role to model id".into(),
        ));
    };
    match object.get(role) {
        None => Err(RoleError::Unknown(role.to_owned())),
        Some(Value::String(id)) if !id.trim().is_empty() => Ok(id.clone()),
        Some(_) => Err(RoleError::Invalid(format!(
            "role {role} must be a non-empty model id"
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
}
