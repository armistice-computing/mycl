//! What a dream cycle runs, and the part of it a person may change.
//!
//! Eschaton's ruling (2026-10-02): dreaming is not fully configurable, but
//! someone who lets Garden run prompts through their own agent stack must be
//! able to see exactly what runs, and to adjust four things: the charge
//! weights, the Dreamer prompt, how many dreams a graph's size earns, and
//! astrology (on by default). Editing sits behind a notice the person agrees
//! to; reading never does. Everything else is house-authored and lives in
//! this file as constants, shown read-only beside the editable fields.
//!
//! Storage is `<profile>/dream-mechanics.json`. Each editable field is an
//! `Option`: `None` means "the house default", so a reset is a `null` and an
//! upgrade that improves a default reaches everyone who never edited it.

use crate::app_runtime::AppHandle;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

const MECHANICS_FILE: &str = "dream-mechanics.json";
const SCHEMA_VERSION: u32 = 1;

/// Bump when the notice's substance changes; an older agreement then no
/// longer opens the editor.
pub(crate) const NOTICE_VERSION: u32 = 1;
pub(crate) const NOTICE: &str = "These settings change how Mycl dreams. The defaults are the ones the house \
dreams with and tests against; we don't recommend changing them. Whatever you change here is shown on every \
dream's receipt, and each field can be reset to its default.";

// ---------------------------------------------------------------------------
// The four editable mechanics, with their house defaults (cloud-1's values).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ChargeWeights {
    pub(crate) embedding: f64,
    pub(crate) structural: f64,
    pub(crate) valuation: f64,
    pub(crate) temporal: f64,
    pub(crate) articulability: f64,
}

pub(crate) const DEFAULT_CHARGE_WEIGHTS: ChargeWeights = ChargeWeights {
    embedding: 0.10,
    structural: 0.25,
    valuation: 0.20,
    temporal: 0.20,
    articulability: 0.25,
};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DreamScaling {
    /// The most dreams one cycle writes for one graph.
    pub(crate) max_dreams_per_cycle: u32,
    /// (documents + wires) / this = scale 1.0.
    pub(crate) scale_reference_point: u32,
    pub(crate) scale_min: f64,
    pub(crate) scale_max: f64,
}

pub(crate) const DEFAULT_SCALING: DreamScaling = DreamScaling {
    max_dreams_per_cycle: 3,
    scale_reference_point: 180,
    scale_min: 0.3,
    scale_max: 3.0,
};

pub(crate) const DEFAULT_ASTROLOGY: bool = true;

/// The temporal layer's curve (see `temporal.rs`): peaks at "just now" and
/// at `cap_days`, zero at `trough_days`. Part of the scoring numbers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TemporalCurve {
    pub(crate) trough_days: f64,
    pub(crate) cap_days: f64,
}

pub(crate) const DEFAULT_TEMPORAL_CURVE: TemporalCurve = TemporalCurve { trough_days: 7.0, cap_days: 180.0 };

/// cloud-1's `_DREAMER_SYSTEM`, verbatim.
pub(crate) const DEFAULT_DREAMER_PROMPT: &str = include_str!("dreamer_prompt.txt");

/// cloud-1's `_BUNDLE_TEMPLATE`. House-authored; shown, not editable.
pub(crate) const BUNDLE_TEMPLATE: &str = "[ATTUNEMENT]\nMoon: {moon_phase}. Sun in {zodiac_sign}.\nCard: {tarot_card} \u{2014} \"{tarot_meaning}\"\n\n[FRAGMENTS]\n{fragments_text}\n\n[DREAM]";
pub(crate) const BUNDLE_TEMPLATE_NO_ASTROLOGY: &str = "[FRAGMENTS]\n{fragments_text}\n\n[DREAM]";

const MAX_PROMPT_CHARS: usize = 32_000;

// ---------------------------------------------------------------------------
// House-authored mechanics (cloud-1's config/dreaming.py defaults).
// ---------------------------------------------------------------------------

pub(crate) const GRAPH_MATURITY_MIN_WIRES: usize = 5;
pub(crate) const GRAPH_MATURITY_MIN_VALUATIONS: usize = 0;
pub(crate) const DISPLACEMENT_TOP_K: f64 = 5.0;
pub(crate) const DISPLACEMENT_NEIGHBORS: usize = 5;
pub(crate) const DISPLACEMENT_NEGATIVE_BAND: (f64, f64) = (0.3, 0.5);
pub(crate) const DISPLACEMENT_POSITIVE_BAND: (f64, f64) = (0.5, 0.7);
pub(crate) const DISPLACEMENT_TWO_HOP_FLOOR: f64 = 0.3;
pub(crate) const CONDENSATION_CLUSTER_MIN: usize = 3;
pub(crate) const CONDENSATION_SAMPLE_SIZE: f64 = 50.0;
pub(crate) const CONDENSATION_SIMILARITY: f64 = 0.65;
pub(crate) const MAX_SIMILARITY: f64 = 0.92;
pub(crate) const WIRE_INFERENCE_MAX_PATHS: f64 = 20.0;
pub(crate) const ISOMORPHISM_MIN_SIMILARITY: f64 = 0.6;
pub(crate) const ISOMORPHISM_MIN_EGO_SIZE: usize = 3;
pub(crate) const ISOMORPHISM_MAX_FINDINGS: f64 = 5.0;
pub(crate) const ISOMORPHISM_MIN_SCORE: f64 = 0.2;
pub(crate) const SCANNER_MAX_PER_DOC: usize = 1;
pub(crate) const NAVEL_FLOOR: f64 = 0.3;
pub(crate) const NAVEL_CEILING: f64 = 0.5;
pub(crate) const WEAVER_MIN_PER_BUNDLE: usize = 2;
pub(crate) const WEAVER_MAX_PER_BUNDLE: usize = 5;
pub(crate) const WEAVER_INCLUDE_NAVEL: bool = true;
pub(crate) const DREAM_MEMORY_ENABLED: bool = true;
pub(crate) const ARTICULATION_ATTEMPTS: u32 = 2;
/// Recently touched valued blocks added to the displacement seeds, so the
/// residue slot has candidates even when old blocks hold the importance.
pub(crate) const RESIDUE_SEEDS: usize = 2;

// ---------------------------------------------------------------------------
// The stored file and the effective view.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Agreement {
    pub(crate) notice_version: u32,
    pub(crate) agreed_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MechanicsEdits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) charge_weights: Option<ChargeWeights>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dreamer_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scaling: Option<DreamScaling>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) astrology_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) temporal_curve: Option<TemporalCurve>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MechanicsFile {
    schema_version: u32,
    #[serde(default)]
    agreement: Option<Agreement>,
    #[serde(default)]
    edits: MechanicsEdits,
}

/// The mechanics one cycle runs with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mechanics {
    pub(crate) charge_weights: ChargeWeights,
    pub(crate) dreamer_prompt: String,
    pub(crate) scaling: DreamScaling,
    pub(crate) astrology_enabled: bool,
    pub(crate) temporal_curve: TemporalCurve,
    /// Names of the fields that differ from the house default, for receipts.
    pub(crate) edited: Vec<&'static str>,
}

impl Default for Mechanics {
    fn default() -> Self {
        MechanicsEdits::default().effective()
    }
}

impl MechanicsEdits {
    pub(crate) fn effective(&self) -> Mechanics {
        let mut edited = Vec::new();
        // An edit equal to its default is not an edit.
        if self.charge_weights.is_some_and(|w| w != DEFAULT_CHARGE_WEIGHTS) {
            edited.push("chargeWeights");
        }
        if self.dreamer_prompt.as_deref().is_some_and(|p| p != DEFAULT_DREAMER_PROMPT) {
            edited.push("dreamerPrompt");
        }
        if self.scaling.is_some_and(|s| s != DEFAULT_SCALING) {
            edited.push("scaling");
        }
        if self.astrology_enabled.is_some_and(|a| a != DEFAULT_ASTROLOGY) {
            edited.push("astrologyEnabled");
        }
        if self.temporal_curve.is_some_and(|c| c != DEFAULT_TEMPORAL_CURVE) {
            edited.push("temporalCurve");
        }
        Mechanics {
            charge_weights: self.charge_weights.unwrap_or(DEFAULT_CHARGE_WEIGHTS),
            dreamer_prompt: self.dreamer_prompt.clone().unwrap_or_else(|| DEFAULT_DREAMER_PROMPT.to_string()),
            scaling: self.scaling.unwrap_or(DEFAULT_SCALING),
            astrology_enabled: self.astrology_enabled.unwrap_or(DEFAULT_ASTROLOGY),
            temporal_curve: self.temporal_curve.unwrap_or(DEFAULT_TEMPORAL_CURVE),
            edited,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Some(w) = self.charge_weights {
            let all = [w.embedding, w.structural, w.valuation, w.temporal, w.articulability];
            if all.iter().any(|v| !v.is_finite() || !(0.0..=1.0).contains(v)) {
                return Err("each charge weight must be between 0 and 1".into());
            }
            if all.iter().sum::<f64>() <= 0.0 {
                return Err("at least one charge weight must be above 0".into());
            }
        }
        if let Some(prompt) = &self.dreamer_prompt {
            if prompt.trim().is_empty() {
                return Err("the Dreamer prompt cannot be empty; reset it to use the default".into());
            }
            if prompt.chars().count() > MAX_PROMPT_CHARS {
                return Err(format!("the Dreamer prompt is limited to {MAX_PROMPT_CHARS} characters"));
            }
        }
        if let Some(c) = self.temporal_curve {
            if !(c.trough_days.is_finite() && c.cap_days.is_finite())
                || c.trough_days < 0.5
                || c.cap_days > 3650.0
                || c.trough_days >= c.cap_days
            {
                return Err("the temporal curve needs 0.5 <= trough < cap <= 3650 days".into());
            }
        }
        if let Some(s) = self.scaling {
            if !(1..=5).contains(&s.max_dreams_per_cycle) {
                return Err("dreams per cycle must be between 1 and 5".into());
            }
            if !(10..=100_000).contains(&s.scale_reference_point) {
                return Err("the scale reference point must be between 10 and 100000".into());
            }
            if !(s.scale_min.is_finite() && s.scale_max.is_finite())
                || s.scale_min < 0.05
                || s.scale_max > 10.0
                || s.scale_min > s.scale_max
            {
                return Err("scale bounds must satisfy 0.05 <= min <= max <= 10".into());
            }
        }
        Ok(())
    }
}

/// cloud-1's `compute_dream_scale`: (documents + wires) / reference, clamped.
pub(crate) fn dream_scale(scaling: &DreamScaling, documents: usize, wires: usize) -> f64 {
    let raw = (documents + wires) as f64 / f64::from(scaling.scale_reference_point.max(1));
    raw.clamp(scaling.scale_min, scaling.scale_max)
}

/// cloud-1's weaver `_compute_max_bundles`: small graphs get fewer dreams.
pub(crate) fn max_bundles_for_scale(scaling: &DreamScaling, scale: f64) -> usize {
    let max = scaling.max_dreams_per_cycle as usize;
    if scale < 0.5 {
        1
    } else if scale < 1.5 {
        max.min(2)
    } else {
        max
    }
}

// ---------------------------------------------------------------------------
// Persistence and the agreement gate.
// ---------------------------------------------------------------------------

fn mechanics_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(crate::profile_paths::profile_dir(app)?.join(MECHANICS_FILE))
}

/// The stored file, or `Err(reason)` when it exists but can't be used. A
/// path error (no profile) is the only hard failure.
fn read_file(app: &AppHandle) -> Result<Result<MechanicsFile, String>, String> {
    let path = mechanics_path(app)?;
    if !path.exists() {
        return Ok(Ok(MechanicsFile { schema_version: SCHEMA_VERSION, ..Default::default() }));
    }
    Ok(crate::storage::read_json::<MechanicsFile>(&path).map_err(|error| error.to_string()))
}

fn agreed(file: &MechanicsFile) -> bool {
    file.agreement.as_ref().is_some_and(|a| a.notice_version == NOTICE_VERSION)
}

/// What a cycle runs with. Edits run only while the current notice is
/// agreed to, so a file edited by hand, or an agreement to an older notice,
/// runs the defaults; so does a file that can't be read or fails validation.
/// The receipt names which.
pub(crate) fn read_mechanics(app: &AppHandle) -> Result<Mechanics, String> {
    Ok(mechanics_from(read_file(app)?))
}

fn mechanics_from(file: Result<MechanicsFile, String>) -> Mechanics {
    let ignored = |why: &'static str| Mechanics { edited: vec![why], ..Mechanics::default() };
    let Ok(file) = file else { return ignored("unreadable-file-ignored") };
    if file.edits.validate().is_err() {
        return ignored("invalid-file-ignored");
    }
    let effective = file.edits.effective();
    if !effective.edited.is_empty() && !agreed(&file) {
        return ignored("edits-ignored-without-agreement");
    }
    effective
}

/// Record that the person read and agreed to the notice.
pub(crate) fn agree_to_notice(app: &AppHandle, notice_version: u32) -> Result<Value, String> {
    if notice_version != NOTICE_VERSION {
        return Err("the notice has changed; read the current one before agreeing".into());
    }
    // An unreadable file is replaced: agreeing is how the person starts over.
    let mut file = read_file(app)?.unwrap_or_default();
    file.schema_version = SCHEMA_VERSION;
    file.agreement = Some(Agreement { notice_version, agreed_at: crate::clock::timestamp() });
    crate::storage::write_json(&mechanics_path(app)?, &file).map_err(|error| error.to_string())?;
    mechanics_view(app)
}

/// Replace the edits. Moving every field back to its default is always
/// allowed; any other change needs the current notice agreed to.
pub(crate) fn set_mechanics(app: &AppHandle, edits: MechanicsEdits) -> Result<Value, String> {
    edits.validate()?;
    // An unreadable file is replaced, so a reset always works from the UI.
    let mut file = read_file(app)?.unwrap_or_default();
    if !edits.effective().edited.is_empty() && !agreed(&file) {
        return Err("agree to the notice before changing how Mycl dreams".into());
    }
    file.schema_version = SCHEMA_VERSION;
    file.edits = edits;
    crate::storage::write_json(&mechanics_path(app)?, &file).map_err(|error| error.to_string())?;
    mechanics_view(app)
}

fn field<T: Serialize>(value: &T, default: &T, edited: bool) -> Value {
    json!({ "value": value, "default": default, "edited": edited })
}

/// Everything a cycle runs with, editable and house-authored alike. Readable
/// without agreeing to anything.
pub(crate) fn mechanics_view(app: &AppHandle) -> Result<Value, String> {
    let file = read_file(app)?;
    let mechanics = read_mechanics(app)?;
    let stored = file.as_ref().ok();
    let mut view = view_of(&mechanics, stored.is_some_and(agreed), stored.and_then(|f| f.agreement.as_ref()));
    // Say why stored edits aren't running, when they aren't.
    if let Some(reason) = mechanics.edited.iter().find(|e| e.contains("ignored")) {
        view["fileStatus"] = json!(reason);
    }
    Ok(view)
}

pub(crate) fn view_of(mechanics: &Mechanics, agreed: bool, agreement: Option<&Agreement>) -> Value {
    let edited = |name: &str| mechanics.edited.contains(&name);
    json!({
        "notice": { "version": NOTICE_VERSION, "text": NOTICE, "agreed": agreed,
                    "agreedAt": agreement.map(|a| a.agreed_at.clone()) },
        "editable": {
            "chargeWeights": field(&mechanics.charge_weights, &DEFAULT_CHARGE_WEIGHTS, edited("chargeWeights")),
            "dreamerPrompt": field(&mechanics.dreamer_prompt, &DEFAULT_DREAMER_PROMPT.to_string(), edited("dreamerPrompt")),
            "scaling": field(&mechanics.scaling, &DEFAULT_SCALING, edited("scaling")),
            "astrologyEnabled": field(&mechanics.astrology_enabled, &DEFAULT_ASTROLOGY, edited("astrologyEnabled")),
            "temporalCurve": field(&mechanics.temporal_curve, &DEFAULT_TEMPORAL_CURVE, edited("temporalCurve")),
        },
        "houseAuthored": house_authored(),
    })
}

fn house_authored() -> Value {
    json!({
        "bundleTemplate": BUNDLE_TEMPLATE,
        "bundleTemplateWithoutAstrology": BUNDLE_TEMPLATE_NO_ASTROLOGY,
        "graphMaturity": { "minWires": GRAPH_MATURITY_MIN_WIRES, "minValuations": GRAPH_MATURITY_MIN_VALUATIONS },
        "displacement": {
            "seedsAtScale1": DISPLACEMENT_TOP_K, "neighbors": DISPLACEMENT_NEIGHBORS,
            "negativeValenceBand": DISPLACEMENT_NEGATIVE_BAND, "positiveValenceBand": DISPLACEMENT_POSITIVE_BAND,
            "twoHopFloor": DISPLACEMENT_TWO_HOP_FLOOR,
        },
        "condensation": { "clusterMin": CONDENSATION_CLUSTER_MIN, "sampleAtScale1": CONDENSATION_SAMPLE_SIZE,
                          "similarity": CONDENSATION_SIMILARITY },
        "nearCopyCeiling": MAX_SIMILARITY,
        "wireInference": { "pathsAtScale1": WIRE_INFERENCE_MAX_PATHS },
        "isomorphism": { "minSignatureSimilarity": ISOMORPHISM_MIN_SIMILARITY, "minEgoSize": ISOMORPHISM_MIN_EGO_SIZE,
                         "findingsAtScale1": ISOMORPHISM_MAX_FINDINGS, "minScore": ISOMORPHISM_MIN_SCORE },
        "maxFindingsPerDocumentPerScanner": SCANNER_MAX_PER_DOC,
        "navel": { "confidenceFloor": NAVEL_FLOOR, "confidenceCeiling": NAVEL_CEILING },
        "weaver": { "minPerDream": WEAVER_MIN_PER_BUNDLE, "maxPerDream": WEAVER_MAX_PER_BUNDLE,
                    "navelSlot": WEAVER_INCLUDE_NAVEL },
        "residueSeeds": RESIDUE_SEEDS,
        "dreamMemory": DREAM_MEMORY_ENABLED,
        "articulationAttempts": ARTICULATION_ATTEMPTS,
        "similarityScale": "Similarities are calibrated to the scale these thresholds were written for (Nomic Embed V2 in cloud-1): each cycle samples random block pairs in this graph's index and maps its scores onto that reference by quantile.",
        "notes": [
            "The temporal layer follows Eschaton's curve (2026-10-02): high for what was touched in the last few days and for what has lain untouched six months or more, lowest at a week. It also nudges which valued blocks seed the displacement scan and which blocks the condensation scan samples. cloud-1 had this layer but never fed it.",
            "Every dream keeps one slot for the strongest finding made partly of material touched within the graph's residue window (five days unless the graph sets another; 0 turns it off), and a few recently touched valued blocks join the displacement seeds so such findings exist. When nothing in the graph is that recent, the slot stays empty.",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untouched_edits_run_the_house_defaults() {
        let m = MechanicsEdits::default().effective();
        assert_eq!(m.charge_weights, DEFAULT_CHARGE_WEIGHTS);
        assert!(m.astrology_enabled);
        assert!(m.edited.is_empty());
        assert!(m.dreamer_prompt.starts_with("You are the Dreamer."));
    }

    #[test]
    fn an_edit_equal_to_its_default_is_not_reported() {
        let edits = MechanicsEdits {
            astrology_enabled: Some(true),
            scaling: Some(DEFAULT_SCALING),
            ..Default::default()
        };
        assert!(edits.effective().edited.is_empty());
        let off = MechanicsEdits { astrology_enabled: Some(false), ..Default::default() };
        assert_eq!(off.effective().edited, vec!["astrologyEnabled"]);
    }

    #[test]
    fn validation_refuses_out_of_range_edits() {
        let bad_weight = MechanicsEdits {
            charge_weights: Some(ChargeWeights { temporal: 1.5, ..DEFAULT_CHARGE_WEIGHTS }),
            ..Default::default()
        };
        assert!(bad_weight.validate().is_err());
        let zero = MechanicsEdits {
            charge_weights: Some(ChargeWeights { embedding: 0.0, structural: 0.0, valuation: 0.0, temporal: 0.0, articulability: 0.0 }),
            ..Default::default()
        };
        assert!(zero.validate().is_err());
        let empty_prompt = MechanicsEdits { dreamer_prompt: Some("  ".into()), ..Default::default() };
        assert!(empty_prompt.validate().is_err());
        let inverted = MechanicsEdits {
            scaling: Some(DreamScaling { scale_min: 2.0, scale_max: 1.0, ..DEFAULT_SCALING }),
            ..Default::default()
        };
        assert!(inverted.validate().is_err());
    }

    #[test]
    fn scale_matches_cloud_examples() {
        // From cloud-1's compute_dream_scale docstring.
        assert!((dream_scale(&DEFAULT_SCALING, 90, 90) - 1.0).abs() < 1e-9);
        assert!((dream_scale(&DEFAULT_SCALING, 35, 33) - 68.0 / 180.0).abs() < 1e-9);
        assert_eq!(dream_scale(&DEFAULT_SCALING, 925, 1061), 3.0);
        assert_eq!(max_bundles_for_scale(&DEFAULT_SCALING, 0.38), 1);
        assert_eq!(max_bundles_for_scale(&DEFAULT_SCALING, 1.0), 2);
        assert_eq!(max_bundles_for_scale(&DEFAULT_SCALING, 3.0), 3);
    }

    #[test]
    fn the_view_shows_every_default_and_the_whole_prompt() {
        let view = view_of(&Mechanics::default(), false, None);
        assert_eq!(view["editable"]["dreamerPrompt"]["default"], DEFAULT_DREAMER_PROMPT);
        assert_eq!(view["editable"]["chargeWeights"]["default"]["temporal"], 0.2);
        assert_eq!(view["houseAuthored"]["bundleTemplate"], BUNDLE_TEMPLATE);
        assert_eq!(view["notice"]["agreed"], false);
    }

    #[test]
    fn edits_run_only_under_the_current_agreement() {
        let edits = MechanicsEdits { astrology_enabled: Some(false), ..Default::default() };
        let file = |agreement: Option<Agreement>| MechanicsFile { schema_version: 1, agreement, edits: edits.clone() };
        // Hand-edited, never agreed: defaults run, and the receipt says why.
        let m = mechanics_from(Ok(file(None)));
        assert!(m.astrology_enabled);
        assert_eq!(m.edited, vec!["edits-ignored-without-agreement"]);
        // Agreed to an older notice: same.
        let old = Agreement { notice_version: NOTICE_VERSION - 1, agreed_at: "then".into() };
        assert!(mechanics_from(Ok(file(Some(old)))).astrology_enabled);
        // Agreed to this notice: the edit runs.
        let now = Agreement { notice_version: NOTICE_VERSION, agreed_at: "now".into() };
        let m = mechanics_from(Ok(file(Some(now))));
        assert!(!m.astrology_enabled);
        assert_eq!(m.edited, vec!["astrologyEnabled"]);
        // A file from a newer build that this one can't parse runs defaults.
        assert_eq!(mechanics_from(Err("unknown field".into())).edited, vec!["unreadable-file-ignored"]);
    }
}
