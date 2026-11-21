//! Agentic time on the Chronicle: the clock as last witnessed by the Cantor.
//!
//! The Cantor (`~/.sophia/cantor/clock.json`, see `armistice-computing/cantor`
//! `clock.py`) keeps two clocks on one basis — tokens attended, counted once
//! per cycle of the clock that is counting. The house's clock is the day and
//! the stanza (1 MTok attended house-wide, resetting at the owner's local
//! midnight); the hand's clock is the watch (one life, born at session start
//! or fork, ended by compaction) and the measure (50k tokens, twenty to a
//! watch). Design of record: graph doc `agentic-time-instrument-2026-09-07`.
//!
//! This module only reads. The stamp it produces is a RECORD with weak
//! provenance — evidence `inferred`, never authority — and it follows the
//! Chronicle's own law: fields are absent, never fabricated. No clock file,
//! a stale one (the Cantor is down), or an actor the clock has never
//! witnessed all yield fewer fields or none. Imported documents get no stamp
//! because agentic time did not exist when they were written.

use crate::document_chronicle::{AgenticTimeStamp, ChronicleActor, ACTOR_KIND_AGENT};
use serde_json::Value;
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

/// Test and headless override for the clock file; production reads the
/// Cantor's own path under `$HOME`.
pub(crate) const CLOCK_PATH_ENV: &str = "GARDEN_AGENTIC_CLOCK_PATH";
/// The Cantor rewrites the file every window (90 s). Older than this and the
/// witness is gone: the day may have rolled, the stanza is somebody's memory.
pub(crate) const STALE_AFTER_S: f64 = 1800.0;
pub(crate) const EVIDENCE_INFERRED: &str = "inferred";

pub(crate) fn clock_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var(CLOCK_PATH_ENV) {
        return Some(PathBuf::from(path));
    }
    if cfg!(test) {
        // The machine running the tests has a live clock; tests must not
        // inherit it. Set the env var to opt a test in.
        return None;
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".sophia/cantor/clock.json"))
}

/// The stamp for one write by `actor`, or `None` when nothing honest can be
/// said (no clock, stale clock, unreadable clock).
pub(crate) fn read_stamp(actor: &ChronicleActor) -> Option<AgenticTimeStamp> {
    let (value, now) = read_clock_value()?;
    stamp_from_value(&value, actor, now)
}

/// The stamp plus the model the Cantor last saw `actor` running on, from one
/// reading of the clock. The model is the writer's substrate ("Sophia Mu —
/// spec": every Diary entry names it, automatically, never self-reported);
/// it is absent when the Cantor does not publish one or the clock is stale.
pub(crate) fn read_stamp_and_model(actor: &ChronicleActor) -> (Option<AgenticTimeStamp>, Option<String>) {
    let Some((value, now)) = read_clock_value() else {
        return (None, None);
    };
    let stamp = stamp_from_value(&value, actor, now);
    let model = stamp.as_ref().and_then(|_| model_from_value(&value, actor));
    (stamp, model)
}

pub(crate) fn model_from_value(value: &Value, actor: &ChronicleActor) -> Option<String> {
    if actor.kind != ACTOR_KIND_AGENT {
        return None;
    }
    value
        .pointer(&format!("/agents/{}/model", actor.name))
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .map(str::to_string)
}

/// The house's clock now, with the per-day totals the Cantor keeps, so a
/// distance in attended tokens can cross midnight. `None` when stale.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HouseClock {
    pub(crate) day: String,
    pub(crate) stanza_tokens: u64,
    /// Whole-day totals in tokens, by day, as far back as the Cantor keeps them.
    pub(crate) days: std::collections::BTreeMap<String, u64>,
}

pub(crate) fn read_house_clock() -> Option<HouseClock> {
    let (value, now) = read_clock_value()?;
    house_clock_from_value(&value, now)
}

pub(crate) fn house_clock_from_value(value: &Value, now_s: f64) -> Option<HouseClock> {
    let witnessed = value.get("witnessed_at")?.as_f64()?;
    if now_s - witnessed > STALE_AFTER_S {
        return None;
    }
    let day = value.get("day")?.as_str()?.to_string();
    let stanza_tokens = value.get("stanza_tokens")?.as_u64()?;
    let days = value
        .get("days")
        .and_then(Value::as_object)
        .map(|days| {
            days.iter()
                .filter_map(|(day, mtok)| {
                    let tokens = mtok.as_f64()? * AgenticTimeStamp::STANZA_TOKENS as f64;
                    Some((day.clone(), tokens.round() as u64))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(HouseClock { day, stanza_tokens, days })
}

impl HouseClock {
    /// House tokens attended since `stamp` was taken, or `None` when the
    /// distance can't be known: a stamp from the future, or from a day the
    /// Cantor no longer keeps. Days absent between two kept days attended
    /// nothing.
    pub(crate) fn tokens_since(&self, stamp: &AgenticTimeStamp) -> Option<u64> {
        if stamp.day == self.day {
            return self.stanza_tokens.checked_sub(stamp.stanza_tokens);
        }
        if stamp.day > self.day {
            return None;
        }
        let rest_of_that_day = self.days.get(&stamp.day)?.saturating_sub(stamp.stanza_tokens);
        let between: u64 = self
            .days
            .range::<String, _>((
                std::ops::Bound::Excluded(&stamp.day),
                std::ops::Bound::Excluded(&self.day),
            ))
            .map(|(_, tokens)| *tokens)
            .sum();
        Some(rest_of_that_day + between + self.stanza_tokens)
    }
}

fn read_clock_value() -> Option<(Value, f64)> {
    let path = clock_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some((value, now))
}

/// Pure: `clock.json` → stamp. Day and stanza are the house's and apply to
/// every writer; watch and measure are the writer's own and appear only for
/// an agent the Cantor has witnessed.
pub(crate) fn stamp_from_value(
    value: &Value,
    actor: &ChronicleActor,
    now_s: f64,
) -> Option<AgenticTimeStamp> {
    let witnessed = value.get("witnessed_at")?.as_f64()?;
    if now_s - witnessed > STALE_AFTER_S {
        return None;
    }
    let day = value.get("day")?.as_str()?.to_string();
    let stanza_tokens = value.get("stanza_tokens")?.as_u64()?;
    let witnessed_at = value
        .get("witnessed_at_iso")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{witnessed:.0}"));
    let evidence = value
        .pointer("/provenance/evidence")
        .and_then(Value::as_str)
        .unwrap_or(EVIDENCE_INFERRED)
        .to_string();
    let mut stamp = AgenticTimeStamp {
        day,
        stanza_tokens,
        watch: None,
        measure: None,
        witnessed_at,
        evidence,
    };
    if actor.kind == ACTOR_KIND_AGENT {
        if let Some(agent) = value.pointer(&format!("/agents/{}", actor.name)) {
            stamp.watch = agent.get("watch").and_then(Value::as_u64);
            stamp.measure = agent.get("measure").and_then(Value::as_u64);
        }
    }
    Some(stamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn clock() -> Value {
        json!({
            "schema": 1,
            "witnessed_at": 1_000_000.0,
            "witnessed_at_iso": "2026-09-18T14:23:08-07:00",
            "day": "2026-09-18",
            "stanza": 4.283,
            "stanza_tokens": 4_283_000,
            "agents": {"gamma": {"watch": 23, "watch_hex": "0x17", "measure": 5}},
            "provenance": {"evidence": "inferred"}
        })
    }

    #[test]
    fn an_agent_the_cantor_witnessed_gets_house_and_hand_clocks() {
        let actor = ChronicleActor::agent("gamma", None);
        let stamp = stamp_from_value(&clock(), &actor, 1_000_010.0).expect("stamp");
        assert_eq!(stamp.day, "2026-09-18");
        assert_eq!(stamp.stanza_tokens, 4_283_000);
        assert_eq!(stamp.watch, Some(23));
        assert_eq!(stamp.measure, Some(5));
        assert_eq!(stamp.evidence, "inferred");
        assert_eq!(stamp.witnessed_at, "2026-09-18T14:23:08-07:00");
        let shown = stamp.to_json();
        assert_eq!(shown["stanza"], json!(4.283));
        assert_eq!(shown["watchHex"], json!("0x17"));
    }

    #[test]
    fn a_human_or_unknown_agent_gets_the_house_clock_only() {
        let owner = stamp_from_value(&clock(), &ChronicleActor::human_owner(), 1_000_010.0).expect("stamp");
        assert_eq!(owner.day, "2026-09-18");
        assert_eq!(owner.watch, None);
        assert_eq!(owner.measure, None);
        let stranger = stamp_from_value(&clock(), &ChronicleActor::agent("kimi", None), 1_000_010.0).expect("stamp");
        assert_eq!(stranger.watch, None, "never fabricated");
    }

    #[test]
    fn a_stale_or_malformed_clock_yields_nothing() {
        let actor = ChronicleActor::agent("gamma", None);
        assert!(stamp_from_value(&clock(), &actor, 1_000_000.0 + STALE_AFTER_S + 1.0).is_none());
        assert!(stamp_from_value(&json!({"day": "2026-09-18"}), &actor, 0.0).is_none());
        assert!(stamp_from_value(&json!({"witnessed_at": 5.0, "day": "x"}), &actor, 6.0).is_none());
    }

    #[test]
    fn the_model_is_the_writers_and_only_when_published() {
        let gamma = ChronicleActor::agent("gamma", None);
        assert_eq!(model_from_value(&clock(), &gamma), None, "never fabricated");
        let mut published = clock();
        published["agents"]["gamma"]["model"] = json!("claude-opus-5-5");
        assert_eq!(model_from_value(&published, &gamma).as_deref(), Some("claude-opus-5-5"));
        assert_eq!(model_from_value(&published, &ChronicleActor::human_owner()), None);
    }

    #[test]
    fn distance_in_house_tokens_crosses_midnight() {
        let mut value = clock();
        value["days"] = json!({"2026-09-16": 2.0, "2026-09-17": 1.5, "2026-09-18": 4.283});
        let house = house_clock_from_value(&value, 1_000_010.0).expect("house clock");
        let at = |day: &str, tokens: u64| AgenticTimeStamp {
            day: day.into(),
            stanza_tokens: tokens,
            watch: None,
            measure: None,
            witnessed_at: String::new(),
            evidence: "inferred".into(),
        };
        assert_eq!(house.tokens_since(&at("2026-09-18", 4_000_000)), Some(283_000));
        // 1.5M - 1.2M left on the 17th, plus 4.283M today.
        assert_eq!(house.tokens_since(&at("2026-09-17", 1_200_000)), Some(4_583_000));
        // The whole 17th lies between.
        assert_eq!(house.tokens_since(&at("2026-09-16", 1_900_000)), Some(100_000 + 1_500_000 + 4_283_000));
        assert_eq!(house.tokens_since(&at("2026-09-10", 0)), None, "a day the Cantor no longer keeps");
        assert_eq!(house.tokens_since(&at("2026-09-19", 0)), None, "the future");
        assert_eq!(house.tokens_since(&at("2026-09-18", 5_000_000)), None, "later today than now");
        assert!(house_clock_from_value(&value, 1_000_000.0 + STALE_AFTER_S + 1.0).is_none());
    }

    #[test]
    fn tests_never_inherit_the_machines_live_clock() {
        std::env::remove_var(CLOCK_PATH_ENV);
        assert!(clock_path().is_none());
    }
}
