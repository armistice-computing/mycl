//! Moon phase, sun sign and a Major Arcana card for the night's dreams,
//! ported from cloud-1's `app/services/dreaming/astrology.py`. Eschaton's
//! ruling (2026-10-02): on by default, because the chance it brings keeps the
//! dreams from settling into repeated images.
//!
//! One deliberate difference: cloud-1 seeded the card with Python's `hash()`,
//! which is salted per process, so its "reproducible" card was in practice
//! random per worker. Here the card is a pure function of seed and date.

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DreamContext {
    pub(crate) moon_phase: String,
    pub(crate) zodiac_sign: String,
    pub(crate) tarot_card: String,
    pub(crate) tarot_meaning: String,
}

const SYNODIC_MONTH: f64 = 29.53058770576;

const MOON_PHASES: [&str; 8] = [
    "new moon",
    "waxing crescent",
    "first quarter",
    "waxing gibbous",
    "full moon",
    "waning gibbous",
    "last quarter",
    "waning crescent",
];

/// Phase at noon UTC of `date`, counted from the new moon of
/// 2000-01-06 18:14 UTC.
pub(crate) fn moon_phase(date: NaiveDate) -> &'static str {
    let reference = NaiveDate::from_ymd_opt(2000, 1, 6)
        .and_then(|day| day.and_hms_opt(18, 14, 0))
        .expect("valid reference date");
    let noon = NaiveDateTime::new(date, NaiveTime::from_hms_opt(12, 0, 0).expect("noon"));
    let days = (noon - reference).num_seconds() as f64 / 86_400.0;
    let fraction = days.rem_euclid(SYNODIC_MONTH) / SYNODIC_MONTH;
    MOON_PHASES[((fraction * 8.0) as usize) % 8]
}

/// (month, day) on which each sign begins.
const ZODIAC: [((u32, u32), &str); 12] = [
    ((1, 20), "Aquarius"),
    ((2, 19), "Pisces"),
    ((3, 21), "Aries"),
    ((4, 20), "Taurus"),
    ((5, 21), "Gemini"),
    ((6, 21), "Cancer"),
    ((7, 23), "Leo"),
    ((8, 23), "Virgo"),
    ((9, 23), "Libra"),
    ((10, 23), "Scorpio"),
    ((11, 22), "Sagittarius"),
    ((12, 22), "Capricorn"),
];

pub(crate) fn zodiac_sign(date: NaiveDate) -> &'static str {
    let day = (date.month(), date.day());
    ZODIAC
        .iter()
        .rev()
        .find(|(start, _)| day >= *start)
        .map(|(_, sign)| *sign)
        .unwrap_or("Capricorn")
}

pub(crate) const MAJOR_ARCANA: [(&str, &str); 22] = [
    ("The Fool", "A step into the unknown with nothing but trust."),
    ("The Magician", "All the elements are present; what matters is the will to combine them."),
    ("The High Priestess", "What is known but not yet spoken sits behind a veil."),
    ("The Empress", "Growth that happens because the conditions were tended, not forced."),
    ("The Emperor", "Structure imposed with enough authority that it feels like ground."),
    ("The Hierophant", "Tradition as a living transmission, not a dead letter."),
    ("The Lovers", "A choice that reveals what you actually value."),
    ("The Chariot", "Opposing forces held together by motion alone."),
    ("Strength", "The hand that opens rather than grips."),
    ("The Hermit", "A lantern carried for the path, not the destination."),
    ("Wheel of Fortune", "What was rising falls; what was falling rises."),
    ("Justice", "The cut that restores proportion."),
    ("The Hanged Man", "Suspension inverts the view; what looked like loss becomes offering."),
    ("Death", "What must end so the next thing can begin."),
    ("Temperance", "Two streams poured into one vessel without either losing its nature."),
    ("The Devil", "The bond that feels like necessity but is only habit."),
    ("The Tower", "Sudden upheaval strips away what was built on false ground."),
    ("The Star", "Bare exposure under open sky \u{2014} vulnerable and enough."),
    ("The Moon", "The path is real but the light that shows it distorts what it illuminates."),
    ("The Sun", "Clarity so complete it leaves nowhere to hide."),
    ("Judgement", "The call that reaches even what was buried."),
    ("The World", "Completion that is also a threshold."),
];

pub(crate) fn tarot_card(date: NaiveDate, seed: &str) -> (&'static str, &'static str) {
    let hash = super::rng::stable_hash(&format!("{seed}|{date}"));
    MAJOR_ARCANA[(hash % MAJOR_ARCANA.len() as u64) as usize]
}

pub(crate) fn dream_context(date: NaiveDate, seed: &str) -> DreamContext {
    let (card, meaning) = tarot_card(date, seed);
    DreamContext {
        moon_phase: moon_phase(date).to_string(),
        zodiac_sign: zodiac_sign(date).to_string(),
        tarot_card: card.to_string(),
        tarot_meaning: meaning.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn matches_cloud_journal_epigraph() {
        // The cloud-1 journal of 2026-09-18 (graph doc default-dream-journal)
        // reads "Moon: waxing crescent. Sun in Virgo." Cloud wrote it with
        // date.today() on a UTC worker during its dream window.
        assert_eq!(moon_phase(day(2026, 9, 18)), "waxing crescent");
        assert_eq!(zodiac_sign(day(2026, 9, 18)), "Virgo");
    }

    #[test]
    fn zodiac_boundaries_follow_cloud_table() {
        assert_eq!(zodiac_sign(day(2026, 1, 19)), "Capricorn");
        assert_eq!(zodiac_sign(day(2026, 1, 20)), "Aquarius");
        assert_eq!(zodiac_sign(day(2026, 12, 21)), "Sagittarius");
        assert_eq!(zodiac_sign(day(2026, 12, 22)), "Capricorn");
    }

    #[test]
    fn card_is_a_pure_function_of_seed_and_date() {
        let a = tarot_card(day(2026, 10, 2), "default_x1");
        assert_eq!(a, tarot_card(day(2026, 10, 2), "default_x1"));
        let spread = (1..=28)
            .map(|d| tarot_card(day(2026, 2, d), "default_x1").0)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(spread.len() > 8, "a month of nights draws many cards: {spread:?}");
    }
}
