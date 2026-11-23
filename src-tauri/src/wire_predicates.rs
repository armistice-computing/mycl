pub(crate) fn predicate_short_name(predicate: &str) -> String {
    predicate
        .rsplit(['#', '/'])
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(predicate)
        .to_string()
}

/// One predicate a person or an agent may choose for a new wire.
pub(crate) struct WirePredicate {
    pub(crate) name: &'static str,
    pub(crate) label: &'static str,
    /// The group it sits in on the window's predicate wheel.
    pub(crate) category: &'static str,
    pub(crate) description: &'static str,
}

/// The question each group answers, as the window's wheel asks it.
pub(crate) const WIRE_PREDICATE_GROUPS: [(&str, &str); 5] = [
    ("Ground", "What makes B real or possible?"),
    ("Critique", "What does A say about B?"),
    ("Genesis", "How does A give rise to B?"),
    ("Structure", "How are A and B ordered or arranged?"),
    ("Generic", "A and B are associated, and no closer word fits."),
];

/// The fourteen judgment predicates: the whole set a new wire may take
/// (Eschaton, 2026-11-21: the set stays small, a retired or unknown name is
/// refused, and no tool makes a new predicate by using one). The names, groups
/// and descriptions match the window's table (shrubbery,
/// packages/nucleus/src/wire-predicates.ts); change both together.
pub(crate) const WIRE_PREDICATES: [WirePredicate; 14] = [
    WirePredicate { name: "grounds", label: "grounds", category: "Ground", description: "A provides the material or experiential foundation that keeps B from being empty abstraction" },
    WirePredicate { name: "requires", label: "requires", category: "Ground", description: "A cannot exist or function without B" },
    WirePredicate { name: "enables", label: "enables", category: "Ground", description: "A creates the conditions for B to be possible" },
    WirePredicate { name: "exemplifies", label: "exemplifies", category: "Ground", description: "A is a concrete instance that makes B real" },
    WirePredicate { name: "supports", label: "supports", category: "Critique", description: "A provides evidence or argument for B" },
    WirePredicate { name: "contradicts", label: "contradicts", category: "Critique", description: "A opposes or undermines B" },
    WirePredicate { name: "qualifies", label: "qualifies", category: "Critique", description: "A modifies, limits, or adds nuance to B" },
    WirePredicate { name: "flowsInto", label: "flows into", category: "Genesis", description: "A continues into or transforms into B" },
    WirePredicate { name: "produces", label: "produces", category: "Genesis", description: "A brought B into existence" },
    WirePredicate { name: "intensifiesWith", label: "intensifies with", category: "Genesis", description: "A and B amplify each other through contact" },
    WirePredicate { name: "contains", label: "contains", category: "Structure", description: "A spatially or structurally includes B" },
    WirePredicate { name: "precedes", label: "precedes", category: "Structure", description: "A comes before B in time or sequence" },
    WirePredicate { name: "consequenceOf", label: "consequence of", category: "Structure", description: "A is an effect or result of B" },
    WirePredicate { name: "relatedTo", label: "is related to", category: "Generic", description: "General association" },
];

/// Predicates the app's own features write through the same path as
/// everyone's wires, each with one fixed meaning: the record family (recalls,
/// cites, authoredBy: provenance, see `WireFamily`), a PDF annotation to its
/// file and page (annotates), and a quotation or a citation brought in from a
/// reference library (quotesFrom, citesEvidence). A feature that needs a new
/// one adds it here; nothing makes one by using it.
pub(crate) const FEATURE_WIRE_PREDICATES: [&str; 6] =
    ["recalls", "cites", "authoredBy", "annotates", "quotesFrom", "citesEvidence"];

/// The predicate a new wire takes when none is named.
pub(crate) const DEFAULT_WIRE_PREDICATE: &str = "relatedTo";

/// The vocabulary the short names belong to.
const VOCAB_NS: &str = "http://mnemosyne.ai/vocab#";

/// The names of the fourteen, for a refusal to list.
pub(crate) fn wire_predicate_names() -> String {
    WIRE_PREDICATES.iter().map(|predicate| predicate.name).collect::<Vec<_>>().join(", ")
}

/// What a new wire stores for `raw`, or None when `raw` is not a predicate a
/// new wire may take. A short name matches whatever its case and is stored in
/// its one spelling ("Supports" is supports, never a second predicate). The
/// vocabulary's own IRI for a name is kept as given. The feature predicates
/// are exact, and they are not offered to anyone as a choice.
pub(crate) fn writable_wire_predicate(raw: &str) -> Option<String> {
    let short = raw.strip_prefix(VOCAB_NS);
    let exact = short.unwrap_or(raw);
    if WIRE_PREDICATES.iter().any(|predicate| predicate.name == exact) || FEATURE_WIRE_PREDICATES.contains(&exact) {
        return Some(raw.to_string());
    }
    if short.is_some() {
        return None;
    }
    WIRE_PREDICATES
        .iter()
        .find(|predicate| predicate.name.eq_ignore_ascii_case(raw))
        .map(|predicate| predicate.name.to_string())
}

pub(crate) fn builtin_wire_predicates() -> Vec<(&'static str, &'static str, &'static str)> {
    WIRE_PREDICATES.iter().map(|predicate| (predicate.name, predicate.label, predicate.category)).collect()
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
    fn a_new_wire_takes_one_of_the_fourteen_in_its_one_spelling() {
        assert_eq!(WIRE_PREDICATES.len(), 14);
        for predicate in &WIRE_PREDICATES {
            assert_eq!(writable_wire_predicate(predicate.name).as_deref(), Some(predicate.name));
            let iri = format!("http://mnemosyne.ai/vocab#{}", predicate.name);
            assert_eq!(writable_wire_predicate(&iri), Some(iri.clone()), "the vocabulary's IRI is kept as given");
            assert!(WIRE_PREDICATE_GROUPS.iter().any(|(group, _)| *group == predicate.category), "{}", predicate.name);
            assert!(!predicate.description.is_empty());
        }
        // A description reads the way the name does: "A requires B" is A's need.
        for predicate in &WIRE_PREDICATES {
            assert!(!predicate.description.starts_with("B "), "{}: {}", predicate.name, predicate.description);
        }
        // Wrong case is the same predicate, never a second one.
        assert_eq!(writable_wire_predicate("Supports").as_deref(), Some("supports"));
        assert_eq!(writable_wire_predicate("FLOWSINTO").as_deref(), Some("flowsInto"));
        // Retired, internal and invented names are not writable.
        for name in ["isWiredTo", "partOf", "causeOf", "divergesFrom", "branchesTo", "consumesWith", "synthesizes", "fooBarBaz", ""] {
            assert_eq!(writable_wire_predicate(name), None, "{name}");
        }
        // No other IRI is, and the vocabulary's IRI is exact.
        assert_eq!(writable_wire_predicate("http://example.test/vocab#supports"), None);
        assert_eq!(writable_wire_predicate("http://mnemosyne.ai/vocab#Supports"), None);
        assert_eq!(writable_wire_predicate("http://mnemosyne.ai/vocab#partOf"), None);
        // Feature predicates are exact and written by the app's own features.
        for name in FEATURE_WIRE_PREDICATES {
            assert_eq!(writable_wire_predicate(name).as_deref(), Some(name));
        }
        assert_eq!(
            writable_wire_predicate("http://mnemosyne.ai/vocab#quotesFrom").as_deref(),
            Some("http://mnemosyne.ai/vocab#quotesFrom")
        );
        assert_eq!(writable_wire_predicate("Cites"), None);
        assert_eq!(writable_wire_predicate("dreamtOf"), None, "only the dream cycle writes it");
        // Retired names still read with their labels on wires that have them.
        assert_eq!(predicate_label("partOf"), "is part of");
        assert_eq!(predicate_label("isWiredTo"), "is wired to");
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
