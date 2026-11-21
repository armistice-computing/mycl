use crate::{
    loopback_scope_catalog::loopback_scope_keys, loopback_token_grants::session_all_token_grant,
};
use std::collections::BTreeSet;

const OWNER_ONLY_SCOPES: &[&str] = &[
    "loopback.manifest.read",
    "loopback.tokens.read",
    "loopback.tokens.write",
    "loopback.tokens.delete",
];

pub(super) fn is_owner_only_scope(scope: &str) -> bool {
    OWNER_ONLY_SCOPES.contains(&scope.trim())
}

pub(super) fn resolve_requested_scopes(
    grant_profile_id: Option<String>,
    scopes: Option<Vec<String>>,
) -> Result<(Option<String>, Vec<String>), String> {
    let known_scopes = loopback_scope_keys().into_iter().collect::<BTreeSet<_>>();
    let grant_profiles = session_all_token_grant().grant_profiles;
    if let Some(grant_profile_id) = grant_profile_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        let Some(profile) = grant_profiles
            .iter()
            .find(|profile| profile.id == grant_profile_id)
        else {
            return Err(format!(
                "unknown loopback grant profile: {grant_profile_id}"
            ));
        };
        if !profile.client_issuable
            || profile
                .scopes
                .iter()
                .any(|scope| is_owner_only_scope(scope))
        {
            return Err(format!(
                "loopback grant profile is owner-only and cannot be issued to a connector: {grant_profile_id}"
            ));
        }
        return Ok((
            Some(grant_profile_id),
            profile
                .scopes
                .iter()
                .map(|scope| (*scope).to_string())
                .collect(),
        ));
    }

    let scopes = scopes.unwrap_or_else(|| {
        grant_profiles
            .iter()
            .find(|profile| profile.id == "read-only")
            .map(|profile| {
                profile
                    .scopes
                    .iter()
                    .map(|scope| (*scope).to_string())
                    .collect()
            })
            .unwrap_or_default()
    });
    let mut normalized = Vec::new();
    let mut seen = BTreeSet::new();
    for scope in scopes {
        let scope = scope.trim().to_string();
        if scope.is_empty() || !seen.insert(scope.clone()) {
            continue;
        }
        if !known_scopes.contains(scope.as_str()) {
            return Err(format!("unknown loopback scope: {scope}"));
        }
        if is_owner_only_scope(&scope) {
            return Err(format!(
                "loopback scope is owner-only and cannot be issued to a connector: {scope}"
            ));
        }
        normalized.push(scope);
    }
    if normalized.is_empty() {
        return Err("loopback client token needs at least one scope".to_string());
    }
    Ok((None, normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_scope_resolution_deduplicates_and_validates() {
        let (_, scopes) = resolve_requested_scopes(
            None,
            Some(vec![
                "graphs.read".to_string(),
                "graphs.read".to_string(),
                "documents.read".to_string(),
            ]),
        )
        .expect("resolve scopes");

        assert_eq!(scopes, vec!["graphs.read", "documents.read"]);
    }

    #[test]
    fn grant_profile_resolution_uses_catalog_scopes() {
        let (profile_id, scopes) = resolve_requested_scopes(
            Some("read-only".to_string()),
            Some(vec!["graphs.delete".to_string()]),
        )
        .expect("resolve profile");

        assert_eq!(profile_id.as_deref(), Some("read-only"));
        assert!(scopes.contains(&"graphs.read".to_string()));
        assert!(!scopes.contains(&"graphs.delete".to_string()));
    }

    #[test]
    fn unknown_scope_is_rejected() {
        let error = resolve_requested_scopes(None, Some(vec!["graphs.fly".to_string()]))
            .expect_err("unknown scope rejected");

        assert!(error.contains("unknown loopback scope"));
    }

    #[test]
    fn explicit_owner_scopes_are_rejected() {
        for scope in OWNER_ONLY_SCOPES {
            let error = resolve_requested_scopes(None, Some(vec![(*scope).to_string()]))
                .expect_err("owner scope rejected");

            assert!(error.contains("owner-only"), "unexpected error: {error}");
        }
    }
}
