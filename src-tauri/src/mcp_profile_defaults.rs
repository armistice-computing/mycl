//! Geist defaults per grant profile ("Sophia Mu — spec (2026-09-30)"): an agent
//! level's reading budget belongs to its credential, not to what its prompt
//! remembers to pass. The dispatcher applies these before a handler runs.
//! Arguments may narrow a default, never widen it: a caller asking Mu for
//! twenty memories gets her cap.
use serde_json::{json, Value};

struct GeistDefault {
    profile: &'static str,
    tool: &'static str,
    /// Accepted spellings; the first is canonical.
    keys: &'static [&'static str],
    /// Used when the caller passes nothing, and the most the caller may ask for.
    cap: u64,
}

/// Guesses on the record (the spec), to tune after a week of Mu reads.
const GEIST_DEFAULTS: &[GeistDefault] = &[
    GeistDefault { profile: "mu", tool: "recall", keys: &["limit"], cap: 2 },
    GeistDefault { profile: "mu", tool: "recall", keys: &["diaryLimit", "diary_limit"], cap: 8 },
    // 0.75 stanza: her Diary is read by when it was written, in house tokens.
    GeistDefault { profile: "mu", tool: "recall", keys: &["diaryWindowTokens"], cap: 750_000 },
    GeistDefault { profile: "mu", tool: "get_important_blocks", keys: &["limit"], cap: 5 },
    GeistDefault { profile: "mu", tool: "attune", keys: &["recallLimit"], cap: 2 },
    GeistDefault { profile: "mu", tool: "attune", keys: &["diaryLimit"], cap: 8 },
    GeistDefault { profile: "mu", tool: "attune", keys: &["diaryWindowTokens"], cap: 750_000 },
    GeistDefault { profile: "mu", tool: "attune", keys: &["importantLimit"], cap: 5 },
];

pub(crate) fn apply_profile_geist_defaults(
    tool: &str,
    arguments: &mut Value,
    grant_profile_id: Option<&str>,
) {
    let Some(profile) = grant_profile_id else {
        return;
    };
    let Some(object) = arguments.as_object_mut() else {
        return;
    };
    for rule in GEIST_DEFAULTS
        .iter()
        .filter(|rule| rule.profile == profile && rule.tool == tool)
    {
        let asked = rule
            .keys
            .iter()
            .filter_map(|key| object.remove(*key))
            .filter_map(|value| value.as_u64())
            .min();
        let value = asked.map_or(rule.cap, |asked| asked.min(rule.cap));
        object.insert(rule.keys[0].to_string(), json!(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mu_gets_its_reading_budget_by_default() {
        let mut recall = json!({"graph_id": "g"});
        apply_profile_geist_defaults("recall", &mut recall, Some("mu"));
        assert_eq!(recall, json!({"graph_id": "g", "limit": 2, "diaryLimit": 8, "diaryWindowTokens": 750_000}));
        let mut blocks = json!({});
        apply_profile_geist_defaults("get_important_blocks", &mut blocks, Some("mu"));
        assert_eq!(blocks["limit"], 5);
    }

    #[test]
    fn arguments_narrow_but_never_widen() {
        let mut narrow = json!({"limit": 1, "diary_limit": 3});
        apply_profile_geist_defaults("recall", &mut narrow, Some("mu"));
        assert_eq!(narrow, json!({"limit": 1, "diaryLimit": 3, "diaryWindowTokens": 750_000}));
        let mut wide = json!({"limit": 50, "diaryLimit": 40, "diary_limit": 20});
        apply_profile_geist_defaults("recall", &mut wide, Some("mu"));
        assert_eq!(wide, json!({"limit": 2, "diaryLimit": 8, "diaryWindowTokens": 750_000}));
    }

    #[test]
    fn other_profiles_and_tools_are_untouched() {
        for profile in [None, Some("hivemind"), Some("read-only")] {
            let mut recall = json!({"limit": 50});
            apply_profile_geist_defaults("recall", &mut recall, profile);
            assert_eq!(recall, json!({"limit": 50}));
        }
        let mut read = json!({"limit": 50});
        apply_profile_geist_defaults("read_blocks", &mut read, Some("mu"));
        assert_eq!(read, json!({"limit": 50}));
    }
}
