//! Dream settings that belong to one graph rather than to the person.
//!
//! The residue window (Eschaton, 2026-10-02): every dream keeps one slot for
//! material touched in the last few days, Freud's day residue. How many days
//! count as "the last few" depends on how fast a graph fills: ours moves
//! daily, a quieter graph may move weekly. Default five days, a work week.
//! Like exclusions, this is setup rather than a mechanic, so it sits outside
//! the agree-to-notice gate; the receipt still shows it.
//!
//! Nightly dreaming is the graph's consent to be dreamed on unattended (plan
//! Phase 3). Off by default. It changes only through `set_nightly`, which
//! stamps when; the scheduler reads it before a cycle and the consent gate
//! reads it again before every send to the dreamer.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub(crate) const DEFAULT_RESIDUE_DAYS: f64 = 5.0;
pub(crate) const MAX_RESIDUE_DAYS: f64 = 60.0;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphSettings {
    /// Days that count as residue. `None` is the default; 0 turns the slot
    /// off (the dream is woven as in cloud-1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) residue_days: Option<f64>,
    /// Consent to nightly dreaming; absent means off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) nightly: Option<NightlyConsent>,
    /// When the computer slept through the night window: `true` dreams once
    /// on the next wake; absent or `false` waits for the next window
    /// (Eschaton, 2026-10-07: the person's choice, default the window only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) catch_up: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NightlyConsent {
    pub(crate) enabled: bool,
    /// When it was last turned on or off.
    pub(crate) changed_at: String,
}

impl GraphSettings {
    pub(crate) fn catch_up(&self) -> bool {
        self.catch_up.unwrap_or(false)
    }

    pub(crate) fn residue_days(&self) -> f64 {
        self.residue_days.unwrap_or(DEFAULT_RESIDUE_DAYS)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Some(days) = self.residue_days {
            if !days.is_finite() || !(0.0..=MAX_RESIDUE_DAYS).contains(&days) {
                return Err(format!("the residue window must be between 0 (off) and {MAX_RESIDUE_DAYS} days"));
            }
        }
        Ok(())
    }

    /// For receipts: the value that ran and whether it is the default.
    pub(crate) fn receipt(&self) -> Value {
        let days = self.residue_days();
        json!({ "residueDays": days, "default": days == DEFAULT_RESIDUE_DAYS, "nightly": self.nightly, "catchUp": self.catch_up() })
    }
}

pub(crate) fn settings_path(graph_dir: &Path) -> PathBuf {
    graph_dir.join("dreaming").join("settings.json")
}

/// The stored settings; defaults when the file is absent, unreadable or out
/// of range (the second value says why it was not used).
pub(crate) fn read_reported(graph_dir: &Path) -> (GraphSettings, Option<String>) {
    let path = settings_path(graph_dir);
    if !path.is_file() {
        return (GraphSettings::default(), None);
    }
    match crate::storage::read_json::<GraphSettings>(&path) {
        Ok(settings) => match settings.validate() {
            Ok(()) => (settings, None),
            Err(error) => (GraphSettings::default(), Some(error)),
        },
        Err(error) => (GraphSettings::default(), Some(error.to_string())),
    }
}

pub(crate) fn read(graph_dir: &Path) -> GraphSettings {
    read_reported(graph_dir).0
}

/// Change the residue window, keeping the nightly consent as it is: consent
/// moves only through `set_nightly`.
pub(crate) fn set_residue_days(graph_dir: &Path, residue_days: Option<f64>) -> Result<GraphSettings, String> {
    let mut settings = read(graph_dir);
    settings.residue_days = residue_days;
    write(graph_dir, &settings)?;
    Ok(settings)
}

/// Whether a missed night is dreamed on the next wake (`None` = default).
pub(crate) fn set_catch_up(graph_dir: &Path, catch_up: Option<bool>) -> Result<GraphSettings, String> {
    let mut settings = read(graph_dir);
    settings.catch_up = catch_up;
    write(graph_dir, &settings)?;
    Ok(settings)
}

/// Turn nightly dreaming on or off for this graph, stamped with the time.
pub(crate) fn set_nightly(graph_dir: &Path, enabled: bool) -> Result<GraphSettings, String> {
    let mut settings = read(graph_dir);
    settings.nightly = Some(NightlyConsent { enabled, changed_at: crate::clock::timestamp() });
    write(graph_dir, &settings)?;
    Ok(settings)
}

fn write(graph_dir: &Path, settings: &GraphSettings) -> Result<(), String> {
    settings.validate()?;
    let path = settings_path(graph_dir);
    crate::storage::create_dir_all(path.parent().ok_or("settings path")?).map_err(|e| e.to_string())?;
    crate::storage::write_json(&path, settings).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_a_work_week_and_zero_turns_it_off() {
        assert_eq!(GraphSettings::default().residue_days(), 5.0);
        let days = |d: f64| GraphSettings { residue_days: Some(d), ..Default::default() };
        assert!(days(0.0).validate().is_ok());
        assert!(days(-1.0).validate().is_err());
        assert!(days(61.0).validate().is_err());
        assert!(days(f64::NAN).validate().is_err());
        assert!(GraphSettings::default().nightly.is_none());
    }

    #[test]
    fn an_out_of_range_file_runs_the_default_and_says_why() {
        let dir = std::env::temp_dir().join(format!("dream-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("dreaming")).unwrap();
        std::fs::write(settings_path(&dir), r#"{"residueDays": 400}"#).unwrap();
        let (settings, error) = read_reported(&dir);
        assert_eq!(settings.residue_days(), DEFAULT_RESIDUE_DAYS);
        assert!(error.is_some());
        set_residue_days(&dir, Some(2.0)).unwrap();
        assert_eq!(read(&dir).residue_days(), 2.0);
        // Consent is stamped, and a residue change leaves it alone.
        set_nightly(&dir, true).unwrap();
        set_residue_days(&dir, Some(3.0)).unwrap();
        let settings = read(&dir);
        let nightly = settings.nightly.unwrap();
        assert!(nightly.enabled);
        assert!(!nightly.changed_at.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
