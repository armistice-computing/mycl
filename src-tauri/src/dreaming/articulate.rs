//! Turning a bundle into the Dreamer's input, and the Dreamer's reply into a
//! dream (cloud-1 `articulator.py`). The call itself goes through
//! `crate::dreamer::call_dreamer`, to whatever dreamer the person chose.

use super::astrology::DreamContext;
use super::mechanics::{BUNDLE_TEMPLATE, BUNDLE_TEMPLATE_NO_ASTROLOGY, NAVEL_CEILING, NAVEL_FLOOR};
use super::scanner::Finding;
use super::weave::Bundle;
use serde_json::Value;
use std::collections::HashMap;

/// cloud-1's `_truncate`: trimmed, newlines flattened, an ellipsis past `max`.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    let text = text.trim().replace('\n', " ");
    if text.chars().count() <= max {
        return text;
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.push('\u{2026}');
    cut
}

fn title<'a>(titles: &'a HashMap<String, String>, doc: &'a str) -> &'a str {
    titles.get(doc).map(String::as_str).unwrap_or(doc)
}

/// The fragments one finding contributes to the Dreamer's input.
pub(crate) fn fragments(finding: &Finding, titles: &HashMap<String, String>) -> String {
    let mut parts = Vec::new();
    match finding {
        Finding::Displacement(f) => {
            parts.push(format!("From \"{}\":\n{}", title(titles, &f.source_doc), f.source_text));
            if let Some((_, doc, text)) = &f.intermediary {
                if !text.is_empty() {
                    parts.push(format!("Through \"{}\":\n{}", title(titles, doc), text));
                }
            }
            parts.push(format!("From \"{}\":\n{}", title(titles, &f.neighbor_doc), f.neighbor_text));
        }
        Finding::Condensation(f) => {
            for b in &f.blocks {
                parts.push(format!("From \"{}\":\n{}", title(titles, &b.doc), b.text));
            }
        }
        Finding::WireInference(f) => {
            let [a, b, c] = &f.titles;
            parts.push(format!(
                "Path: \"{a}\" \u{2014}[{}]\u{2192} \"{b}\" \u{2014}[{}]\u{2192} \"{c}\"",
                f.predicate_ab, f.predicate_bc
            ));
            parts.push(format!("No direct wire between \"{a}\" and \"{c}\"."));
            if !f.block_a_text.is_empty() {
                parts.push(format!("From \"{a}\": {}", truncate(&f.block_a_text, 120)));
            }
            if !f.block_c_text.is_empty() {
                parts.push(format!("From \"{c}\": {}", truncate(&f.block_c_text, 120)));
            }
        }
        Finding::Isomorphism(f) => {
            let matched = f.matched.iter().map(|m| format!("[{}]", m.predicate)).collect::<Vec<_>>().join(", ");
            let matched = if matched.is_empty() { "shared shape".to_string() } else { matched };
            parts.push(format!(
                "\"{}\" and \"{}\" share the same relational shape ({matched}). Structural similarity: {}. Semantic distance: {}.",
                f.title_a, f.title_b, percent(f.structural_similarity), percent(f.semantic_distance)
            ));
        }
    }
    parts.join("\n\n")
}

/// Python's `{:.0%}`.
pub(crate) fn percent(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

/// The user message for one dream: the attunement (when astrology is on)
/// and every finding's fragments.
pub(crate) fn bundle_prompt(bundle: &Bundle, context: Option<&DreamContext>, titles: &HashMap<String, String>) -> String {
    let fragments_text = bundle
        .findings
        .iter()
        .map(|r| fragments(&r.finding, titles))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    match context {
        Some(ctx) => BUNDLE_TEMPLATE
            .replace("{moon_phase}", &ctx.moon_phase)
            .replace("{zodiac_sign}", &ctx.zodiac_sign)
            .replace("{tarot_card}", &ctx.tarot_card)
            .replace("{tarot_meaning}", &ctx.tarot_meaning)
            .replace("{fragments_text}", &fragments_text),
        None => BUNDLE_TEMPLATE_NO_ASTROLOGY.replace("{fragments_text}", &fragments_text),
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Articulation {
    /// scene, navel or veto, as the Dreamer answered.
    pub(crate) kind: String,
    pub(crate) narrative: String,
    pub(crate) confidence: f64,
    pub(crate) is_navel: bool,
}

impl Articulation {
    /// A dream the journal keeps: at or above the navel floor, with words.
    pub(crate) fn accepted(&self) -> bool {
        self.confidence >= NAVEL_FLOOR && !self.narrative.trim().is_empty()
    }
}

/// cloud-1's `_parse_llm_result` and `_classify_entry`: the type bounds the
/// confidence (a veto at most 0.1, a navel within 0.3..0.49), and a dream in
/// the navel band is a navel whatever it called itself.
pub(crate) fn parse_reply(reply: &Value) -> Articulation {
    let kind = reply.get("type").and_then(Value::as_str).unwrap_or("scene").to_string();
    let narrative = reply.get("narrative").and_then(Value::as_str).unwrap_or("").to_string();
    let mut confidence = match reply.get("confidence") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.5),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.5),
        _ => 0.5,
    };
    if !confidence.is_finite() {
        confidence = 0.5;
    }
    match kind.as_str() {
        "veto" => confidence = confidence.min(0.1),
        "navel" => confidence = confidence.clamp(0.3, 0.49),
        _ => {}
    }
    Articulation { is_navel: NAVEL_FLOOR <= confidence && confidence < NAVEL_CEILING, kind, narrative, confidence }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dreaming::mechanics::DEFAULT_DREAMER_PROMPT;
    use crate::dreaming::scanner::{Displacement, Isomorphism, WireInference};
    use crate::dreaming::weave::Ranked;
    use serde_json::json;

    #[test]
    fn prompt_constants_are_cloud_verbatim() {
        assert!(DEFAULT_DREAMER_PROMPT.starts_with("You are the Dreamer. You are not awake. You do not analyze, explain, or recommend. You show."));
        assert!(DEFAULT_DREAMER_PROMPT.contains("- NAVEL: {\"type\": \"navel\", \"narrative\": \"...\", \"confidence\": 0.3-0.5}"));
        assert!(DEFAULT_DREAMER_PROMPT.ends_with("Output: {\"type\": \"veto\", \"confidence\": 0.0}"));
    }

    #[test]
    fn replies_are_bounded_by_their_type() {
        let navel = parse_reply(&json!({"type": "navel", "narrative": "A clock with no hands\u{2026}", "confidence": 0.9}));
        assert_eq!(navel.confidence, 0.49);
        assert!(navel.is_navel && navel.accepted());
        let veto = parse_reply(&json!({"type": "veto", "confidence": 0.8}));
        assert_eq!(veto.confidence, 0.1);
        assert!(!veto.accepted());
        let scene = parse_reply(&json!({"type": "scene", "narrative": "Rain.", "confidence": "0.76"}));
        assert_eq!(scene.confidence, 0.76);
        assert!(!scene.is_navel && scene.accepted());
        // A scene in the navel band is classified as a navel, as in cloud-1.
        assert!(parse_reply(&json!({"type": "scene", "narrative": "x", "confidence": 0.4})).is_navel);
        // No words, no dream.
        assert!(!parse_reply(&json!({"type": "scene", "confidence": 0.9})).accepted());
    }

    #[test]
    fn fragments_follow_cloud_formats() {
        let titles: HashMap<String, String> = [("d1".to_string(), "on-being-given-room".to_string())].into();
        let d = Finding::Displacement(Displacement {
            source_block: "b1".into(), source_doc: "d1".into(), source_text: "theosis".into(), source_importance: 4.8,
            neighbor_block: "b3".into(), neighbor_doc: "d3".into(), neighbor_text: "a chain of theosis".into(),
            similarity: 0.4, intermediary: Some(("b2".into(), "d2".into(), "grace".into())), hops: 2,
        });
        assert_eq!(fragments(&d, &titles), "From \"on-being-given-room\":\ntheosis\n\nThrough \"d2\":\ngrace\n\nFrom \"d3\":\na chain of theosis");
        let w = Finding::WireInference(WireInference {
            docs: ["a".into(), "b".into(), "c".into()], titles: ["A".into(), "B".into(), "C".into()],
            predicate_ab: "supports".into(), predicate_bc: "contradicts".into(), inferred_predicate: "contradicts".into(),
            structural_confidence: 0.5, requires_llm_judgment: true, composition_note: String::new(),
            block_a: String::new(), block_b: String::new(), block_c: String::new(),
            block_a_text: "x".repeat(130), block_c_text: String::new(),
        });
        let text = fragments(&w, &titles);
        assert!(text.starts_with("Path: \"A\" \u{2014}[supports]\u{2192} \"B\" \u{2014}[contradicts]\u{2192} \"C\"\n\nNo direct wire between \"A\" and \"C\"."));
        assert!(text.ends_with(&format!("From \"A\": {}\u{2026}", "x".repeat(119))));
        let i = Finding::Isomorphism(Isomorphism {
            doc_a: "a".into(), doc_b: "b".into(), title_a: "A".into(), title_b: "B".into(),
            structural_similarity: 1.0, semantic_distance: 0.38, score: 0.38, matched: Vec::new(),
        });
        assert_eq!(fragments(&i, &titles), "\"A\" and \"B\" share the same relational shape (shared shape). Structural similarity: 100%. Semantic distance: 38%.");
    }

    #[test]
    fn bundle_prompt_carries_the_attunement_only_when_astrology_is_on() {
        let finding = Finding::Isomorphism(Isomorphism {
            doc_a: "a".into(), doc_b: "b".into(), title_a: "A".into(), title_b: "B".into(),
            structural_similarity: 0.5, semantic_distance: 0.5, score: 0.25, matched: Vec::new(),
        });
        let charge = crate::dreaming::weave::score(Default::default(), &crate::dreaming::mechanics::DEFAULT_CHARGE_WEIGHTS);
        let bundle = Bundle { findings: vec![Ranked { finding, charge, residue: false }], ..Default::default() };
        let ctx = DreamContext { moon_phase: "waxing crescent".into(), zodiac_sign: "Virgo".into(),
                                 tarot_card: "Justice".into(), tarot_meaning: "The cut that restores proportion.".into() };
        let with = bundle_prompt(&bundle, Some(&ctx), &HashMap::new());
        assert!(with.starts_with("[ATTUNEMENT]\nMoon: waxing crescent. Sun in Virgo.\nCard: Justice \u{2014} \"The cut that restores proportion.\"\n\n[FRAGMENTS]\n\"A\" and \"B\""));
        assert!(with.ends_with("\n\n[DREAM]"));
        assert!(bundle_prompt(&bundle, None, &HashMap::new()).starts_with("[FRAGMENTS]\n\"A\""));
    }
}
