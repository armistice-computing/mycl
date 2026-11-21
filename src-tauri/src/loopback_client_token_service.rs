use crate::app_runtime::AppHandle;
use crate::{
    clock::{epoch_millis, timestamp},
    loopback_audit_log::append_loopback_audit_event,
    loopback_client_token_audit::token_audit_event,
    loopback_client_token_records::{
        expires_at_from_days, hash_loopback_token, token_record_matches, token_summary,
    },
    loopback_client_token_scope_resolution::{is_owner_only_scope, resolve_requested_scopes},
    loopback_client_token_store::{read_token_store, update_token_store},
    loopback_client_token_types::{
        CreateLoopbackClientTokenInput, CreateLoopbackClientTokenResponse, ReissueLoopbackClientTokenResponse,
        LoopbackClientTokenRecord, LoopbackClientTokenStore, LoopbackClientTokenSummary,
        ResolvedLoopbackClientToken, RevokeLoopbackClientTokenInput,
    },
};
use uuid::Uuid;

const HOME_GRAPH_TTL_MILLIS: u128 = 24 * 60 * 60 * 1000;

pub(crate) fn list_client_tokens(
    app: &AppHandle,
) -> Result<Vec<LoopbackClientTokenSummary>, String> {
    let now = epoch_millis();
    Ok(read_token_store(app)?
        .tokens
        .iter()
        .map(|record| {
            let mut summary = token_summary(record, now);
            summary.active &= !record_requires_reissue(record);
            summary
        })
        .collect())
}

pub(crate) fn create_client_token(
    app: &AppHandle,
    input: CreateLoopbackClientTokenInput,
) -> Result<CreateLoopbackClientTokenResponse, String> {
    create_client_token_as(app, input, "tauri")
}

pub(crate) fn create_client_token_as(
    app: &AppHandle,
    input: CreateLoopbackClientTokenInput,
    actor: &str,
) -> Result<CreateLoopbackClientTokenResponse, String> {
    let CreateLoopbackClientTokenInput {
        label,
        grant_profile_id,
        scopes,
        expires_in_days,
    } = input;
    let label = label
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Loopback client".to_string());
    let (grant_profile_id, scopes) = resolve_requested_scopes(grant_profile_id, scopes)?;
    let now = epoch_millis();
    let expires_at = expires_at_from_days(now, expires_in_days)?;
    let token = format!("sophia-lb-{}", Uuid::new_v4().simple());
    let record = LoopbackClientTokenRecord {
        token_id: Uuid::new_v4().to_string(),
        label,
        token_hash: hash_loopback_token(&token),
        grant_profile_id,
        scopes,
        home_graph_id: None,
        home_graph_set_at: None,
        created_at: now.to_string(),
        expires_at,
        revoked_at: None,
    };

    update_token_store(app, |store| {
        store.tokens.push(record.clone());
        Ok(())
    })?;
    append_token_audit_best_effort(app, "create", actor, &record);

    Ok(CreateLoopbackClientTokenResponse {
        token,
        record: token_summary(&record, now),
    })
}

pub(crate) fn revoke_client_token(
    app: &AppHandle,
    input: RevokeLoopbackClientTokenInput,
) -> Result<LoopbackClientTokenSummary, String> {
    revoke_client_token_as(app, input, "tauri")
}

pub(crate) fn revoke_client_token_as(
    app: &AppHandle,
    input: RevokeLoopbackClientTokenInput,
    actor: &str,
) -> Result<LoopbackClientTokenSummary, String> {
    let token_id = input.token_id.trim();
    let (summary, audit_event) = update_token_store(app, |store| {
        let Some(record) = store
            .tokens
            .iter_mut()
            .find(|record| record.token_id == token_id)
        else {
            return Err(format!("loopback client token not found: {token_id}"));
        };
        let outcome = if record.revoked_at.is_none() {
            "succeeded"
        } else {
            "unchanged"
        };
        if record.revoked_at.is_none() {
            record.revoked_at = Some(timestamp());
        }
        let summary = token_summary(record, epoch_millis());
        let audit_event = token_audit_event("revoke", outcome, actor, record);
        Ok((summary, audit_event))
    })?;
    if let Err(error) = append_loopback_audit_event(app, &audit_event) {
        log::debug!(
            "failed to append loopback token revoke audit event for {}: {error}",
            summary.token_id
        );
    }
    Ok(summary)
}

/// Re-issue a connector in one step: a new secret under the same label,
/// grant profile and scopes, the old record revoked in the same store
/// transaction, the new token returned once. The expiry length of the
/// original is kept unless `expires_in_days` says otherwise; the home
/// graph does not carry over (the new key sets its own). A revoked or
/// expired record can be re-issued: that is what the button is for.
pub(crate) fn reissue_client_token_as(
    app: &AppHandle,
    token_id: &str,
    expires_in_days: Option<u32>,
    actor: &str,
) -> Result<ReissueLoopbackClientTokenResponse, String> {
    let token_id = token_id.trim().to_string();
    let now = epoch_millis();
    let token = format!("sophia-lb-{}", Uuid::new_v4().simple());
    let token_hash = hash_loopback_token(&token);
    let (old, new) = update_token_store(app, |store| {
        let Some(old) = store
            .tokens
            .iter_mut()
            .find(|record| record.token_id == token_id)
        else {
            return Err(format!("loopback client token not found: {token_id}"));
        };
        if record_requires_reissue(old) {
            // A legacy owner-grade record is retired, never cloned: its
            // scopes would come back with it.
            return Err(format!(
                "loopback client token {token_id} holds owner-only scopes and cannot be re-issued; mint a new connector"
            ));
        }
        let days = match expires_in_days {
            Some(days) => Some(days),
            None => original_ttl_days(old),
        };
        let expires_at = expires_at_from_days(now, days)?;
        if old.revoked_at.is_none() {
            old.revoked_at = Some(timestamp());
        }
        let new = LoopbackClientTokenRecord {
            token_id: Uuid::new_v4().to_string(),
            label: old.label.clone(),
            token_hash: token_hash.clone(),
            grant_profile_id: old.grant_profile_id.clone(),
            scopes: old.scopes.clone(),
            home_graph_id: None,
            home_graph_set_at: None,
            created_at: now.to_string(),
            expires_at,
            revoked_at: None,
        };
        let old = old.clone();
        store.tokens.push(new.clone());
        Ok((old, new))
    })?;
    append_token_audit_best_effort(app, "reissue-revoke", actor, &old);
    append_token_audit_best_effort(app, "reissue-create", actor, &new);
    Ok(ReissueLoopbackClientTokenResponse {
        token,
        record: token_summary(&new, now),
        previous_token_id: old.token_id,
    })
}

/// The original's lifetime in whole days, so a re-issue keeps it; None
/// (the default) when the record's stamps cannot be read.
fn original_ttl_days(record: &LoopbackClientTokenRecord) -> Option<u32> {
    let created = record.created_at.parse::<u128>().ok()?;
    let expires = record.expires_at.parse::<u128>().ok()?;
    let days = expires.checked_sub(created)? / (24 * 60 * 60 * 1000);
    u32::try_from(days).ok().filter(|days| *days >= 1)
}

fn append_token_audit_best_effort(
    app: &AppHandle,
    action: &str,
    actor: &str,
    record: &LoopbackClientTokenRecord,
) {
    if let Err(error) =
        append_loopback_audit_event(app, &token_audit_event(action, "succeeded", actor, record))
    {
        log::debug!(
            "failed to append loopback token {action} audit event for {}: {error}",
            record.token_id
        );
    }
}

pub(crate) fn resolve_loopback_client_token(
    app: &AppHandle,
    token: &str,
) -> Result<Option<Vec<String>>, String> {
    Ok(resolve_loopback_client_token_authorization(app, token)?.map(|auth| auth.scopes))
}

/// Resolve the complete live named principal once so authorization and
/// Chronicle attribution derive from the same token record.
pub(crate) fn resolve_loopback_client_token_authorization(
    app: &AppHandle,
    token: &str,
) -> Result<Option<ResolvedLoopbackClientToken>, String> {
    let store = read_token_store(app)?;
    Ok(resolve_stored_token_authorization(
        &store,
        token,
        epoch_millis(),
    ))
}

fn record_requires_reissue(record: &LoopbackClientTokenRecord) -> bool {
    record.grant_profile_id.as_deref().map(str::trim) == Some("session-all")
        || record.scopes.iter().any(|scope| is_owner_only_scope(scope))
}

fn resolve_stored_token_authorization(
    store: &LoopbackClientTokenStore,
    token: &str,
    now: u128,
) -> Option<ResolvedLoopbackClientToken> {
    let token_hash = hash_loopback_token(token);
    store
        .tokens
        .iter()
        .find(|record| token_record_matches(record, &token_hash, now))
        .filter(|record| !record_requires_reissue(record))
        .map(|record| ResolvedLoopbackClientToken {
            token_id: record.token_id.clone(),
            label: record.label.clone(),
            // A scope-only key inside garden's scopes is a garden key: the
            // ceiling is inferred here, the one place a record becomes an
            // authorization, so every reader (tool list, dispatcher, REST)
            // sees the same profile.
            grant_profile_id: crate::loopback_token_grants::effective_grant_profile_id(
                record.grant_profile_id.as_deref(),
                &record.scopes,
            ),
            scopes: record.scopes.clone(),
            home_graph_id: active_home_graph(record, now),
        })
}

fn active_home_graph(record: &LoopbackClientTokenRecord, now: u128) -> Option<String> {
    let set_at = record.home_graph_set_at.as_deref()?.parse::<u128>().ok()?;
    if now.saturating_sub(set_at) > HOME_GRAPH_TTL_MILLIS {
        return None;
    }
    record
        .home_graph_id
        .clone()
        .filter(|value| !value.trim().is_empty())
}

pub(crate) fn set_loopback_client_home_graph(
    app: &AppHandle,
    token_id: &str,
    graph_id: Option<&str>,
) -> Result<Option<String>, String> {
    let normalized = graph_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(graph_id) = normalized.as_deref() {
        crate::graph_service::read_graph_record(app, graph_id)
            .map_err(crate::app_error::AppError::message)?;
    }

    update_token_store(app, |store| {
        update_home_graph_record(store, token_id, normalized, epoch_millis())
    })
}

fn update_home_graph_record(
    store: &mut LoopbackClientTokenStore,
    token_id: &str,
    normalized: Option<String>,
    now: u128,
) -> Result<Option<String>, String> {
    let record = store
        .tokens
        .iter_mut()
        .find(|record| record.token_id == token_id)
        .ok_or_else(|| format!("loopback client token not found: {token_id}"))?;
    if !token_record_matches(record, &record.token_hash, now) || record_requires_reissue(record) {
        return Err("loopback client token is inactive or requires reissue".to_string());
    }
    record.home_graph_id = normalized.clone();
    record.home_graph_set_at = normalized.as_ref().map(|_| now.to_string());
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_ttl_is_read_in_whole_days_and_unreadable_stamps_fall_back() {
        let mut record = token_with_home(None, None);
        record.created_at = "0".into();
        record.expires_at = (30u128 * 24 * 60 * 60 * 1000).to_string();
        assert_eq!(original_ttl_days(&record), Some(30));
        record.expires_at = "invalid".into();
        assert_eq!(original_ttl_days(&record), None);
        record.expires_at = "0".into();
        assert_eq!(original_ttl_days(&record), None, "a zero-day lifetime falls back to the default");
    }

    fn stored_token() -> LoopbackClientTokenStore {
        let mut record = token_with_home(Some("original"), Some(10));
        record.token_hash = hash_loopback_token("test-secret");
        record.scopes = vec!["documents.read".into(), "mcp.tools.call".into()];
        record.expires_at = "2000".into();
        LoopbackClientTokenStore {
            tokens: vec![record],
        }
    }

    #[test]
    fn stored_legacy_owner_credentials_require_reissue_without_downgrading() {
        for scope in [
            "loopback.manifest.read",
            "loopback.tokens.read",
            "loopback.tokens.write",
            "loopback.tokens.delete",
        ] {
            let mut store = stored_token();
            store.tokens[0].scopes.push(scope.into());
            // Exercise the persisted representation, without any profile access.
            let bytes = serde_json::to_vec(&store).unwrap();
            let loaded = serde_json::from_slice(&bytes).unwrap();
            assert!(resolve_stored_token_authorization(&loaded, "test-secret", 1000).is_none());
            assert_eq!(serde_json::to_vec(&loaded).unwrap(), bytes);
        }
        let mut store = stored_token();
        store.tokens[0].grant_profile_id = Some("session-all".into());
        assert!(resolve_stored_token_authorization(&store, "test-secret", 1000).is_none());
    }

    #[test]
    fn stored_normal_credentials_keep_their_exact_scopes_and_require_live_token() {
        for profile in [None, Some("read-only"), Some("hivemind")] {
            let mut store = stored_token();
            store.tokens[0].grant_profile_id = profile.map(str::to_string);
            let auth = resolve_stored_token_authorization(&store, "test-secret", 1999).unwrap();
            assert_eq!(auth.scopes, store.tokens[0].scopes);
            // A scope-only key whose scopes sit inside garden's is read as a
            // garden key (the ceiling, 2026-10-30); a profiled key keeps its own.
            let expected = match profile {
                None => Some(crate::loopback_token_grants::GARDEN_PROFILE_ID),
                other => other,
            };
            assert_eq!(auth.grant_profile_id.as_deref(), expected);
            assert!(resolve_stored_token_authorization(&store, "wrong-secret", 1999).is_none());
            assert!(resolve_stored_token_authorization(&store, "test-secret", 2000).is_none());
            store.tokens[0].revoked_at = Some("1000".into());
            assert!(resolve_stored_token_authorization(&store, "test-secret", 1999).is_none());
        }
    }

    /// A scope-only key that asks for a scope outside garden's set keeps
    /// its own shape: no ceiling is inferred for it.
    #[test]
    fn a_scope_only_key_outside_gardens_scopes_is_not_capped() {
        let mut store = stored_token();
        store.tokens[0].grant_profile_id = None;
        store.tokens[0].scopes.push("rdf.update".into());
        let auth = resolve_stored_token_authorization(&store, "test-secret", 1999).unwrap();
        assert_eq!(auth.grant_profile_id, None);
    }

    #[test]
    fn home_graph_mutation_rejects_inactive_or_legacy_tokens_without_changes() {
        for reason in [
            "revoked",
            "expired",
            "malformed-expiry",
            "owner-scope",
            "session-all",
        ] {
            let mut store = stored_token();
            match reason {
                "revoked" => store.tokens[0].revoked_at = Some("1000".into()),
                "expired" => store.tokens[0].expires_at = "1000".into(),
                "malformed-expiry" => store.tokens[0].expires_at = "invalid".into(),
                "owner-scope" => store.tokens[0].scopes.push("loopback.manifest.read".into()),
                "session-all" => store.tokens[0].grant_profile_id = Some("session-all".into()),
                _ => unreachable!(),
            }
            let before = serde_json::to_vec(&store).unwrap();
            for home in [Some("new-home".to_string()), None] {
                assert!(
                    update_home_graph_record(&mut store, "token-id", home, 1000).is_err(),
                    "{reason}"
                );
                assert_eq!(serde_json::to_vec(&store).unwrap(), before);
            }
        }
    }

    #[test]
    fn live_home_graph_mutations_set_and_clear_and_unknown_token_fails() {
        let mut store = stored_token();
        assert_eq!(
            update_home_graph_record(&mut store, "token-id", Some("new-home".into()), 1000)
                .unwrap(),
            Some("new-home".into())
        );
        assert_eq!(store.tokens[0].home_graph_set_at.as_deref(), Some("1000"));
        assert_eq!(
            update_home_graph_record(&mut store, "token-id", None, 1001).unwrap(),
            None
        );
        assert_eq!(store.tokens[0].home_graph_set_at, None);
        assert!(update_home_graph_record(&mut store, "unknown-token", None, 1001).is_err());
    }

    fn token_with_home(
        home_graph_id: Option<&str>,
        set_at: Option<u128>,
    ) -> LoopbackClientTokenRecord {
        LoopbackClientTokenRecord {
            token_id: "token-id".to_string(),
            label: "Epsilon".to_string(),
            token_hash: "hash".to_string(),
            grant_profile_id: Some("hivemind".to_string()),
            scopes: vec![],
            home_graph_id: home_graph_id.map(str::to_string),
            home_graph_set_at: set_at.map(|value| value.to_string()),
            created_at: "0".to_string(),
            expires_at: "9999999999999".to_string(),
            revoked_at: None,
        }
    }

    #[test]
    fn home_graph_is_active_for_twenty_four_hours() {
        let now = HOME_GRAPH_TTL_MILLIS + 10;
        let record = token_with_home(Some("default"), Some(10));
        assert_eq!(active_home_graph(&record, now), Some("default".to_string()));
        assert_eq!(active_home_graph(&record, now + 1), None);
    }

    #[test]
    fn missing_malformed_or_empty_home_graph_state_fails_closed() {
        let mut record = token_with_home(Some("default"), None);
        assert_eq!(active_home_graph(&record, 10), None);

        record.home_graph_set_at = Some("not-a-timestamp".to_string());
        assert_eq!(active_home_graph(&record, 10), None);

        record.home_graph_set_at = Some("10".to_string());
        record.home_graph_id = Some("   ".to_string());
        assert_eq!(active_home_graph(&record, 10), None);
    }
}
