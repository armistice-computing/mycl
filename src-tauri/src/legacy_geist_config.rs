//! Current legacy Geist configuration only. Past documents remain graph documents:
//! their independent histories cannot establish coherent combined config snapshots.

use crate::salience_value_config::{default_value_weights, LocalValueConfig};
use serde_json::Value;
use std::collections::BTreeMap;

const PRESENT: [&str; 3] = ["geist-importance", "geist-valence", "geist-weights"];

/// Accept canonical TipTap `doc` values, not persisted document-record envelopes.
/// A partial trio is an error, never an invitation to replace a missing prompt.
pub(crate) fn parse_config(
    documents: &BTreeMap<String, Value>,
) -> Result<Option<LocalValueConfig>, String> {
    if !PRESENT.iter().any(|id| documents.contains_key(*id)) {
        return Ok(None);
    }
    let mut rendered = Vec::new();
    for id in PRESENT {
        let doc = documents
            .get(id)
            .ok_or_else(|| format!("incomplete legacy Geist config: missing {id}"))?;
        let text = markdown(doc).map_err(|error| format!("{id}: {error}"))?;
        if text.trim().is_empty() {
            return Err(format!("{id}: empty configuration document"));
        }
        rendered.push(text);
    }
    let weights = parse_weights(&rendered[2])?;
    Ok(Some(LocalValueConfig {
        importance_prompt: rendered[0].clone(),
        valence_prompt: rendered[1].clone(),
        temporal_half_life_days: weights["half_life_days"],
        weights,
    }))
}

fn parse_weights(text: &str) -> Result<BTreeMap<String, f64>, String> {
    // These keys and defaults match mnemosyne-mcp geist.py DEFAULT_WEIGHTS.
    let mut weights = default_value_weights();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let error = |reason: &str| format!("geist-weights line {}: {reason}: {line}", index + 1);
        // Preserve the legacy parser's colon-before-equals precedence.
        let (key, value) = line
            .split_once(':')
            .or_else(|| line.split_once('='))
            .ok_or_else(|| error("expected key: value or key=value"))?;
        let key = key.trim();
        if !weights.contains_key(key) {
            return Err(error("unsupported weight key"));
        }
        let value: f64 = value
            .trim()
            .parse()
            .map_err(|_| error("invalid numeric value"))?;
        if !value.is_finite() || value < 0.0 {
            return Err(error("numeric value must be finite and nonnegative"));
        }
        if (key.ends_with("_ref") || key == "half_life_days") && value <= 0.0 {
            return Err(error("reference values and half-life must be positive"));
        }
        if key == "workspace_depth" && value.fract() != 0.0 {
            return Err(error("workspace_depth must be an integer"));
        }
        // Legacy duplicates use the last valid assignment.
        weights.insert(key.to_string(), value);
    }
    Ok(weights)
}

fn children(node: &Value) -> Result<&[Value], String> {
    match node.get("content") {
        None => Ok(&[]),
        Some(Value::Array(nodes)) => Ok(nodes),
        _ => Err("content must be an array".to_string()),
    }
}

/// Deliberately narrow port of converters/xml_to_markdown.py. Unsupported shapes
/// fail instead of flattening formatting away. The imported present trio uses
/// only headings, paragraphs and unmarked text (including float heading levels).
fn markdown(doc: &Value) -> Result<String, String> {
    if doc["type"] != "doc" {
        return Err("expected canonical TipTap doc".to_string());
    }
    let mut lines = Vec::new();
    for block in children(doc)? {
        let text = inline(block)?;
        match block["type"].as_str() {
            Some("paragraph") => lines.push(text),
            Some("heading") => {
                let level = match block.pointer("/attrs/level") {
                    None => 1.0,
                    Some(value) => value
                        .as_f64()
                        .or_else(|| match value.as_str()? {
                            "1" => Some(1.0),
                            "2" => Some(2.0),
                            "3" => Some(3.0),
                            "4" => Some(4.0),
                            "5" => Some(5.0),
                            "6" => Some(6.0),
                            _ => None,
                        })
                        .ok_or(
                            "heading level must be numeric or a legacy digit from 1 through 6",
                        )?,
                };
                if !level.is_finite() || level < 1.0 || level.fract() != 0.0 {
                    return Err("heading level must be a positive integer".to_string());
                }
                lines.push(format!("{} {text}", "#".repeat(level.min(6.0) as usize)));
            }
            other => return Err(format!("unsupported block type: {other:?}")),
        }
        lines.push(String::new());
    }
    // Legacy converter collapses 3+ newlines even within text, trims only
    // boundary newlines, then appends one. Do not trim wording or spaces.
    let mut result = String::new();
    let mut newlines = 0;
    for ch in lines.join("\n").chars() {
        newlines = if ch == '\n' { newlines + 1 } else { 0 };
        if newlines <= 2 {
            result.push(ch);
        }
    }
    Ok(format!("{}\n", result.trim_matches('\n')))
}

fn inline(block: &Value) -> Result<String, String> {
    let mut result = String::new();
    for node in children(block)? {
        if node["type"] == "hardBreak" {
            result.push_str("  \n");
            continue;
        }
        if node["type"] != "text" {
            return Err(format!("unsupported inline type: {}", node["type"]));
        }
        let text = node["text"].as_str().ok_or("text node requires text")?;
        let marks = match node.get("marks") {
            None => &[][..],
            Some(Value::Array(marks)) => marks.as_slice(),
            _ => return Err("marks must be an array".to_string()),
        };
        // Multiple marks require the legacy XML writer's nesting order. Do not
        // guess at that order; the present corpus has no multiply-marked text.
        if marks.len() > 1 {
            return Err("multiple inline marks are unsupported".to_string());
        }
        let rendered = match marks.first() {
            None => text.to_string(),
            Some(mark) => match mark["type"].as_str() {
                Some("bold" | "strong") => format!("**{text}**"),
                Some("italic" | "em") => format!("*{text}*"),
                Some("strike") => format!("~~{text}~~"),
                Some("code") if text.contains('`') => format!("`` {text} ``"),
                Some("code") => format!("`{text}`"),
                Some("highlight") => format!("=={text}=="),
                Some("commentMark") => text.to_string(),
                Some("link") => {
                    let href = mark
                        .pointer("/attrs/href")
                        .and_then(Value::as_str)
                        .ok_or("link mark requires href")?;
                    format!("[{text}]({href})")
                }
                other => return Err(format!("unsupported inline mark: {other:?}")),
            },
        };
        result.push_str(&rendered);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(text: &str) -> Value {
        json!({"type":"doc", "content":[
            {"type":"paragraph", "content":[{"type":"text", "text":text}]}
        ]})
    }

    fn trio(weights: &str) -> BTreeMap<String, Value> {
        BTreeMap::from([
            (PRESENT[0].to_string(), doc("Importance wording")),
            (PRESENT[1].to_string(), doc("Valence wording")),
            (PRESENT[2].to_string(), doc(weights)),
        ])
    }

    #[test]
    fn legacy_heading_levels_render_without_changing_text() {
        for level in 1..=6 {
            let numeric = json!({"type":"doc","content":[{"type":"heading","attrs":{"level":level},"content":[{"type":"text","text":"Configuration"}]}]});
            let mut legacy = numeric.clone();
            legacy["content"][0]["attrs"]["level"] = json!(level.to_string());
            assert_eq!(markdown(&numeric).unwrap(), markdown(&legacy).unwrap());
        }
        for level in [
            json!("1.0"),
            json!("0"),
            json!("7"),
            json!(" 1"),
            json!(true),
            Value::Null,
        ] {
            let doc = json!({"type":"doc","content":[{"type":"heading","attrs":{"level":level}}]});
            assert!(markdown(&doc).is_err());
        }
    }

    #[test]
    fn absent_and_history_only_are_not_current_config() {
        assert!(parse_config(&BTreeMap::new()).unwrap().is_none());
        let history = BTreeMap::from([("geist-past-importance".to_string(), Value::Null)]);
        assert!(parse_config(&history).unwrap().is_none());
    }

    #[test]
    fn requires_each_member_of_trio() {
        for id in PRESENT {
            let mut docs = trio("half_life_days: 60");
            docs.remove(id);
            assert!(parse_config(&docs).unwrap_err().contains(id));
        }
    }

    #[test]
    fn maps_weights_defaults_and_half_life_without_touching_history() {
        let mut docs = trio(
            "# Scoring Configuration\nimportance_weight: 0.3\n\
            temporal_weight = 0.25\ndoc_wires_weight: 0.05\n\
            wire_freshness_weight: 0.05\nhalf_life_days: 60.0",
        );
        docs.insert("geist-past-weights".to_string(), doc("half_life_days: 7"));
        let original = docs.clone();
        let config = parse_config(&docs).unwrap().unwrap();
        assert_eq!(config.importance_prompt, "Importance wording\n");
        assert_eq!(config.valence_prompt, "Valence wording\n");
        assert_eq!(config.temporal_half_life_days, 60.0);
        assert_eq!(config.weights["half_life_days"], 60.0);
        assert_eq!(config.weights["temporal_weight"], 0.25);
        assert_eq!(config.weights["doc_wires_weight"], 0.05);
        assert_eq!(config.weights["wire_freshness_weight"], 0.05);
        assert_eq!(config.weights["importance_ref"], 10.0);
        assert_eq!(config.weights["workspace_depth"], 2.0);
        assert_eq!(docs, original);
        let defaults = parse_config(&trio("importance_weight=0.4\nimportance_weight:0.5"))
            .unwrap()
            .unwrap();
        assert_eq!(defaults.weights["importance_weight"], 0.5);
        assert_eq!(defaults.temporal_half_life_days, 7.0);
        assert_eq!(defaults.weights["half_life_days"], 7.0);
    }

    #[test]
    fn strict_weights_never_silently_skip_bad_assignments() {
        for text in [
            "unknown: 1",
            "not an assignment",
            "importance_weight: nope",
            "importance_weight: NaN",
            "importance_weight: inf",
            "importance_weight: 1e999",
            "importance_weight: -0.1",
            "half_life_days: 0",
            "valence_ref: -1",
            "workspace_depth: 1.5",
            "half_life_days=60: ignored",
            "half_life_days:",
        ] {
            assert!(parse_config(&trio(text)).is_err(), "{text}");
        }
    }

    #[test]
    fn renderer_matches_legacy_spacing_float_headings_and_marks() {
        let input = json!({"type":"doc", "content":[
            {"type":"heading", "attrs":{"level":1.0}, "content":[
                {"type":"text", "text":"Importance Prompt"}]},
            {"type":"paragraph", "content":[
                {"type":"text", "text":"Keep *literal* & wording. "},
                {"type":"text", "text":"bold", "marks":[{"type":"bold"}]},
                {"type":"hardBreak"},
                {"type":"text", "text":"a`b", "marks":[{"type":"code"}]}]},
            {"type":"paragraph"}, {"type":"paragraph"},
            {"type":"paragraph", "content":[{"type":"text", "text":" final "}]}
        ]});
        assert_eq!(
            markdown(&input).unwrap(),
            "# Importance Prompt\n\nKeep *literal* & wording. **bold**  \n`` a`b ``\n\n final \n"
        );
    }

    #[test]
    fn rejects_empty_malformed_and_unsupported_prompt_shapes() {
        for input in [
            json!({"type":"doc"}),
            json!({"type":"doc", "content":{}}),
            json!({"type":"doc", "content":[{"type":"image"}]}),
            json!({"type":"doc", "content":[{"type":"bulletList", "content":[]}]}),
            json!({"type":"doc", "content":[{"type":"paragraph", "content":[
                {"type":"text", "text":"x", "marks":[{"type":"bold"},{"type":"italic"}]}]}]}),
            json!({"tiptapJson":{"type":"doc"}}),
        ] {
            let mut docs = trio("half_life_days: 60");
            docs.insert(PRESENT[0].to_string(), input);
            assert!(parse_config(&docs).unwrap_err().contains(PRESENT[0]));
        }
    }
}
