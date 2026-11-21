pub(crate) fn predicate_short_name(predicate: &str) -> String {
    predicate
        .rsplit(['#', '/'])
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(predicate)
        .to_string()
}

pub(crate) fn builtin_wire_predicates() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("isWiredTo", "is wired to", ""),
        ("partOf", "is part of", "Quantity"),
        ("contains", "contains", "Quantity"),
        ("exemplifies", "is an example of", "Quantity"),
        ("supports", "supports", "Quality"),
        ("contradicts", "contradicts", "Quality"),
        ("qualifies", "qualifies", "Quality"),
        ("causeOf", "causes", "Relation"),
        ("consequenceOf", "is consequence of", "Relation"),
        ("relatedTo", "is related to", "Relation"),
        ("requires", "requires", "Modality"),
        ("enables", "enables", "Modality"),
        ("precedes", "precedes", "Modality"),
        ("flowsInto", "flows into", "Synthesis"),
        ("produces", "produces", "Synthesis"),
        ("divergesFrom", "diverges from", "Synthesis"),
        ("branchesTo", "branches to", "Synthesis"),
        ("consumesWith", "consumes with", "Synthesis"),
        ("intensifiesWith", "intensifies with", "Synthesis"),
    ]
}

/// The predicate a dream cycle writes from its journal to what it dreamt of.
pub(crate) const DREAMT_OF: &str = "dreamtOf";
/// How long a dreamtOf wire lives: one day's residue.
pub(crate) const DREAM_WIRE_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

/// What kind of relation a wire records, and the rules that follow from it.
///
/// Judgment wires (supports, contradicts, flowsInto, every builtin and any
/// unknown or custom predicate) are a reader's claim about how two things
/// relate: they shape salience, traversal and dreaming. Record wires
/// (recalls, cites, authoredBy) state objective provenance and never make a
/// thing weigh more by accumulation (Eschaton, Geist 2.1, 2026-07-18; the
/// 2026-09-10 ruling that the exclusion be executable on every counting
/// path). Dream wires (dreamtOf) are the day's residue of a dream: they
/// count toward salience, expire after a day and are written only by the
/// dream cycle, which also deletes them (Eschaton, 2026-10-19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireFamily {
    Judgment,
    Record,
    Dream,
}

impl WireFamily {
    /// Read per wire in every salience pass, so it borrows rather than
    /// allocates. Names are exact: "Cites" or "references" are judgment.
    pub(crate) fn of(predicate: &str) -> Self {
        let short = predicate.rsplit(['#', '/']).next().filter(|s| !s.is_empty()).unwrap_or(predicate);
        match short.trim() {
            "recalls" | "cites" | "authoredBy" => WireFamily::Record,
            DREAMT_OF => WireFamily::Dream,
            _ => WireFamily::Judgment,
        }
    }

    pub(crate) fn of_wire(wire: &serde_json::Value) -> Self {
        Self::of(wire.get("predicate").and_then(serde_json::Value::as_str).unwrap_or_default())
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            WireFamily::Judgment => "judgment",
            WireFamily::Record => "record",
            WireFamily::Dream => "dream",
        }
    }

    pub(crate) fn counts_toward_salience(self) -> bool {
        self != WireFamily::Record
    }

    /// A wire of this family is deleted this long after it is written.
    pub(crate) fn lifetime_ms(self) -> Option<i64> {
        match self {
            WireFamily::Dream => Some(DREAM_WIRE_LIFETIME_MS),
            _ => None,
        }
    }

    /// Only the server writes these; tools and the interface are refused.
    pub(crate) fn server_written(self) -> bool {
        self == WireFamily::Dream
    }

    /// Part of the graph's conceptual structure, as dreaming reads it.
    pub(crate) fn conceptual(self) -> bool {
        self == WireFamily::Judgment
    }
}

fn legacy_wire_predicate_label(short_name: &str) -> Option<&'static str> {
    match short_name {
        "recalls" => Some("recalls"),
        "cites" => Some("cites"),
        "authoredBy" => Some("is authored by"),
        DREAMT_OF => Some("dreamt of"),
        "isWiredTo" => Some("is wired to"),
        "causeOf" => Some("causes"),
        "partOf" => Some("is part of"),
        "divergesFrom" => Some("diverges from"),
        "branchesTo" => Some("branches to"),
        "consumesWith" => Some("consumes with"),
        "synthesizes" => Some("synthesizes"),
        _ => None,
    }
}

pub(crate) fn predicate_label(predicate: &str) -> String {
    let short_name = predicate_short_name(predicate);
    builtin_wire_predicates()
        .into_iter()
        .find(|(name, _, _)| *name == short_name)
        .map(|(_, label, _)| label.to_string())
        .or_else(|| legacy_wire_predicate_label(&short_name).map(str::to_string))
        .unwrap_or_else(|| short_name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_helpers_preserve_builtin_legacy_and_uri_labels() {
        assert_eq!(
            predicate_short_name("http://mnemosyne.ai/vocab#supports"),
            "supports"
        );
        assert_eq!(predicate_short_name("urn:custom/predicate"), "predicate");
        assert_eq!(predicate_label("supports"), "supports");
        assert_eq!(predicate_label("causeOf"), "causes");
        assert_eq!(predicate_label("synthesizes"), "synthesizes");
        assert_eq!(predicate_label("customRelation"), "customRelation");
    }

    #[test]
    fn families_follow_the_predicate_and_carry_their_rules() {
        assert_eq!(WireFamily::of("supports"), WireFamily::Judgment);
        assert_eq!(WireFamily::of("someCustomRelation"), WireFamily::Judgment);
        assert_eq!(WireFamily::of("http://mnemosyne.ai/vocab#cites"), WireFamily::Record);
        assert_eq!(WireFamily::of("recalls"), WireFamily::Record);
        assert_eq!(WireFamily::of("authoredBy"), WireFamily::Record);
        assert_eq!(WireFamily::of("http://mnemosyne.ai/vocab#dreamtOf"), WireFamily::Dream);
        assert!(!WireFamily::Record.counts_toward_salience());
        assert!(WireFamily::Dream.counts_toward_salience());
        assert_eq!(WireFamily::Dream.lifetime_ms(), Some(DREAM_WIRE_LIFETIME_MS));
        assert_eq!(WireFamily::Record.lifetime_ms(), None);
        assert!(WireFamily::Dream.server_written() && !WireFamily::Record.server_written());
        assert!(WireFamily::Judgment.conceptual() && !WireFamily::Dream.conceptual() && !WireFamily::Record.conceptual());
        assert_eq!(predicate_label("dreamtOf"), "dreamt of");
    }
}
