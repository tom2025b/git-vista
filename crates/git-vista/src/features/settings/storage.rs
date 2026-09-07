//! Browser persistence policy, independent of the DOM (ADR 0135).
//! Export deliberately enumerates and validates three non-sensitive toggles;
//! drafts, repository identifiers, tracking records and future keys stay out.

pub const EXPORT_KEYS: [&str; 3] = [
    "git-vista.icons",
    "git-vista.node-icons",
    "git-vista.collapse-wip",
];

pub fn export_preferences(entries: &[(String, String)]) -> String {
    let preferences: serde_json::Map<String, serde_json::Value> = entries
        .iter()
        .filter(|(key, value)| match key.as_str() {
            "git-vista.icons" => matches!(value.as_str(), "nerd" | "text"),
            "git-vista.node-icons" | "git-vista.collapse-wip" => {
                matches!(value.as_str(), "on" | "off")
            }
            _ => false,
        })
        .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
        .collect();
    serde_json::json!({"format": "git-vista-preferences", "version": 1, "preferences": preferences})
        .to_string()
}

/// Includes legacy session drafts. Never clear another application's storage.
pub fn owns_storage_key(key: &str) -> bool {
    key.starts_with("git-vista.") || key.starts_with("gv-commit-draft:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_excludes_repository_data_and_unrecognized_preference_values() {
        let entries = [
            ("git-vista.icons", "text"),
            ("git-vista.node-icons", "private-token-canary"),
            ("git-vista.collapse-wip", "off"),
            ("git-vista.inflight-remote-op", "private-branch-canary"),
            ("git-vista.comparison", "private-repo-canary"),
            ("gv-commit-draft:repo", "private-draft-canary"),
            ("git-vista.future", "private-future-canary"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let exported: serde_json::Value =
            serde_json::from_str(&export_preferences(&entries)).unwrap();
        assert_eq!(
            exported,
            serde_json::json!({
                "format": "git-vista-preferences", "version": 1,
                "preferences": {"git-vista.icons": "text", "git-vista.collapse-wip": "off"}
            })
        );
        assert_eq!(EXPORT_KEYS.len(), 3);
    }

    #[test]
    fn clearing_includes_persistent_and_legacy_drafts_but_preserves_foreign_keys() {
        for key in [
            "git-vista.icons",
            "git-vista.comparison",
            "git-vista.inflight-remote-op",
            "gv-commit-draft:repo",
        ] {
            assert!(owns_storage_key(key), "{key}");
        }
        for key in [
            "other-app",
            "git-vista-other",
            "gv-commit-draft-other",
            "token",
        ] {
            assert!(!owns_storage_key(key), "{key}");
        }
    }
}
