//! When the dreamed material was last lived with. Eschaton's design
//! (2026-10-02), after Freud: dreams draw mostly on the last couple of days
//! (the day's residue) and on the oldest material, with everything between a
//! tier down. cloud-1 had a "temporal depth" layer meant to reward the old
//! only, and never fed it.
//!
//! The curve has equal peaks: 1 at "just now", falling fast to 0 at the
//! trough (a week), then climbing back to 1 at the cap (six months) and
//! staying there. The near side is a quadratic in days; the far side is a
//! quadratic in log-days, so the climb back is slow.
//!
//! "Last touched" is the later of the document's last edit and the block's
//! last valuation. Edits are recorded per document, so one edited line makes
//! a whole long document recent.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use super::mechanics::TemporalCurve;

pub(crate) const DAY_MS: f64 = 86_400_000.0;

/// The curve's value at `days` since the material was last touched.
pub(crate) fn curve(days: f64, c: &TemporalCurve) -> f64 {
    let days = days.max(0.0);
    if days < c.trough_days {
        let x = (c.trough_days - days) / c.trough_days;
        return x * x;
    }
    if days >= c.cap_days {
        return 1.0;
    }
    let x = (days / c.trough_days).ln() / (c.cap_days / c.trough_days).ln();
    (x * x).min(1.0)
}

/// Epoch milliseconds from a stored timestamp: a millisecond number (or its
/// string), or RFC 3339. Anything before 2000 or more than a day in the
/// future is treated as unknown (imports carry some corrupt dates).
pub(crate) fn parse_ms(value: &serde_json::Value, now_ms: i64) -> Option<i64> {
    let ms = match value {
        serde_json::Value::Number(n) => n.as_f64().map(|f| f as i64),
        serde_json::Value::String(s) => parse_ms_str(s),
        _ => None,
    }?;
    (946_684_800_000..=now_ms + DAY_MS as i64).contains(&ms).then_some(ms)
}

pub(crate) fn parse_ms_str(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(f) = s.parse::<f64>() {
        return Some(f as i64);
    }
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp_millis())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentTimes {
    #[serde(default)]
    updated_at: serde_json::Value,
}

/// Each document's last edit, read from its record. Only `updatedAt` is kept;
/// serde skips the rest of each file without allocating it.
pub(crate) fn document_edit_times(graph_dir: &Path, now_ms: i64) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(graph_dir.join("documents")) else { return out };
    for entry in entries.flatten() {
        let Some(id) = entry.file_name().to_str().map(str::to_string) else { continue };
        let Ok(file) = std::fs::File::open(entry.path().join("document.json")) else { continue };
        let reader = std::io::BufReader::with_capacity(1 << 16, file);
        if let Ok(times) = serde_json::from_reader::<_, DocumentTimes>(reader) {
            if let Some(ms) = parse_ms(&times.updated_at, now_ms) {
                out.insert(id, ms);
            }
        }
    }
    out
}

/// The curve over a graph: last-touched times for documents and valued blocks.
pub(crate) struct Clock<'a> {
    pub(crate) curve: &'a TemporalCurve,
    pub(crate) now_ms: i64,
    pub(crate) doc_edited: &'a HashMap<String, i64>,
    /// block id → last valuation (ms).
    pub(crate) block_valued: HashMap<&'a str, i64>,
}

impl Clock<'_> {
    pub(crate) fn last_touched(&self, doc: &str, block: Option<&str>) -> Option<i64> {
        let edited = self.doc_edited.get(doc).copied();
        let valued = block.and_then(|b| self.block_valued.get(b).copied());
        edited.max(valued)
    }

    /// Days since this block (or document) was last touched, if known.
    pub(crate) fn age_days(&self, doc: &str, block: Option<&str>) -> Option<f64> {
        self.last_touched(doc, block).map(|ms| ((self.now_ms - ms) as f64 / DAY_MS).max(0.0))
    }

    /// The curve at this block (or document); unknown age scores 0.
    pub(crate) fn temporal(&self, doc: &str, block: Option<&str>) -> f64 {
        self.age_days(doc, block).map(|days| curve(days, self.curve)).unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dreaming::mechanics::DEFAULT_TEMPORAL_CURVE as C;

    #[test]
    fn the_curve_has_two_equal_peaks_and_a_trough_at_a_week() {
        let at = |d: f64| (curve(d, &C) * 100.0).round() / 100.0;
        assert_eq!(at(0.0), 1.0);
        assert_eq!(at(1.0), 0.73);
        assert_eq!(at(2.0), 0.51);
        assert_eq!(at(3.0), 0.33);
        assert_eq!(at(7.0), 0.0);
        assert_eq!(at(14.0), 0.05);
        assert_eq!(at(30.0), 0.2);
        assert_eq!(at(90.0), 0.62);
        assert_eq!(at(180.0), 1.0);
        assert_eq!(at(3000.0), 1.0);
        // Continuous at the trough, monotone on each side.
        assert!(curve(6.999, &C) < 1e-6 && curve(7.001, &C) < 1e-6);
        let left: Vec<f64> = (0..70).map(|i| curve(i as f64 / 10.0, &C)).collect();
        assert!(left.windows(2).all(|w| w[0] >= w[1]));
        let right: Vec<f64> = (7..400).map(|d| curve(d as f64, &C)).collect();
        assert!(right.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn timestamps_parse_in_both_stored_forms_and_corrupt_ones_are_unknown() {
        let now = 1_791_000_000_000;
        assert_eq!(parse_ms(&serde_json::json!(1773792596296_i64), now), Some(1773792596296));
        assert_eq!(parse_ms(&serde_json::json!("2026-10-02T14:37:14-07:00"), now), Some(1790977034000));
        assert_eq!(parse_ms(&serde_json::json!("1773792596296"), now), Some(1773792596296));
        // default_x1 holds createdAt -58159440422000 on one imported document.
        assert_eq!(parse_ms(&serde_json::json!(-58159440422000_i64), now), None);
        assert_eq!(parse_ms(&serde_json::json!(now + 10 * DAY_MS as i64), now), None);
        assert_eq!(parse_ms(&serde_json::Value::Null, now), None);
    }

    #[test]
    fn a_block_is_as_recent_as_its_latest_touch() {
        let now = 1_790_000_000_000_i64;
        let edited: HashMap<String, i64> = [("d".to_string(), now - 200 * DAY_MS as i64)].into();
        let clock = Clock {
            curve: &C,
            now_ms: now,
            doc_edited: &edited,
            block_valued: [("b", now - DAY_MS as i64)].into(),
        };
        // Edited long ago, valued yesterday: yesterday wins.
        assert!((clock.temporal("d", Some("b")) - 0.73).abs() < 0.01);
        // Another block of the same document: deep past.
        assert_eq!(clock.temporal("d", Some("other")), 1.0);
        assert_eq!(clock.temporal("unknown", None), 0.0);
    }
}
