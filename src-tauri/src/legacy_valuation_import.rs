//! Lossless cloud-1 N-Quads valuation import into the native shared commons.
//! Installation requires the caller's graph write exclusion (including live valuations).

use crate::{
    ids::validate_local_id,
    salience_value_store::{
        block_value_key, read_value_store, LocalBlockValueRecord, LocalValueStore,
    },
    storage::{create_dir_all, write_json},
};
use oxigraph::{
    io::{RdfFormat, RdfParser},
    model::{GraphName, NamedOrBlankNode, Term},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

const DOC: &str = "http://mnemosyne.dev/doc#";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

#[derive(Debug, Default)]
pub struct LegacyValuationPlan {
    pub records: BTreeMap<String, LocalBlockValueRecord>,
    pub quarantine: BTreeMap<String, Value>,
}

/// Retain every selected subject's quad semantics, even when it cannot become a native row.
pub fn plan_legacy_valuations(
    rdf: &str,
    source_user: &str,
    source_graph: &str,
) -> Result<LegacyValuationPlan, String> {
    validate_local_id(source_user, "source user")?;
    validate_local_id(source_graph, "source graph")?;
    let root = format!("urn:mnemosyne:user:{source_user}:graph:{source_graph}");
    let prefix = format!("{root}:valuation:");
    let reference_prefix = format!("{root}:doc:");
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut targets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(rdf.as_bytes()) {
        let quad = quad.map_err(|e| format!("invalid legacy valuation RDF: {e}"))?;
        if !matches!(&quad.graph_name, GraphName::NamedNode(g) if g.as_str() == root) {
            continue;
        }
        let NamedOrBlankNode::NamedNode(subject) = &quad.subject else {
            continue;
        };
        if !subject.as_str().starts_with(&prefix) {
            continue;
        }
        let subject = subject.as_str().to_owned();
        groups
            .entry(subject.clone())
            .or_default()
            .push(format!("{quad} .\n"));
        // Even an otherwise invalid row can collide with a valid row's explicit blockRef.
        if quad.predicate.as_str() == format!("{DOC}blockRef") {
            if let Term::NamedNode(reference) = &quad.object {
                if let Some((doc, block)) = reference
                    .as_str()
                    .strip_prefix(&reference_prefix)
                    .and_then(|s| s.split_once("#block-"))
                {
                    if validate_local_id(doc, "document ID").is_ok()
                        && validate_local_id(block, "block ID").is_ok()
                    {
                        targets
                            .entry(block_value_key(doc, block))
                            .or_default()
                            .insert(subject);
                    }
                }
            }
        }
    }
    let mut plan = LegacyValuationPlan::default();
    for (subject, quads) in &mut groups {
        quads.sort();
        let rdf = quads.concat();
        match parse_legacy_valuations(&rdf, source_user, source_graph) {
            Ok(records) => plan.records.extend(records),
            Err(reason) => {
                plan.quarantine.insert(
                    subject.clone(),
                    serde_json::json!({"rdf": rdf, "reason": reason}),
                );
            }
        }
    }
    for (key, subjects) in targets.iter().filter(|(_, subjects)| subjects.len() > 1) {
        plan.records.remove(key);
        for subject in subjects {
            let collision = format!("multiple legacy valuation subjects target block {key}");
            let entry = plan.quarantine.entry(subject.clone()).or_insert_with(
                || serde_json::json!({"rdf": groups[subject].concat(), "reason": ""}),
            );
            let previous = entry["reason"].as_str().unwrap_or("");
            entry["reason"] = Value::String(if previous.is_empty() {
                collision
            } else {
                format!("{previous}; {collision}")
            });
        }
    }
    Ok(plan)
}

/// Caller must hold the same graph write exclusion as for native valuation installation.
pub fn install_quarantine(
    graph_dir: &Path,
    quarantine: &BTreeMap<String, Value>,
) -> Result<(), String> {
    if quarantine.is_empty() {
        return Ok(());
    }
    for (subject, entry) in quarantine {
        if !entry["rdf"].is_string() || !entry["reason"].is_string() {
            return Err(format!("invalid valuation quarantine entry for {subject}"));
        }
    }
    let path = graph_dir.join("values/legacy-valuation-quarantine.json");
    let read = || -> Result<Option<Vec<u8>>, String> {
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read {}: {e}", path.display())),
        }
    };
    let original = read()?;
    let mut json: Value = match &original {
        Some(bytes) => serde_json::from_slice(bytes)
            .map_err(|e| format!("invalid valuation quarantine: {e}"))?,
        None => serde_json::json!({"schemaVersion": 1, "records": {}}),
    };
    if json["schemaVersion"].as_u64() != Some(1) {
        return Err("unsupported valuation quarantine schemaVersion".into());
    }
    let records = json
        .get_mut("records")
        .and_then(Value::as_object_mut)
        .ok_or("valuation quarantine records must be an object")?;
    let mut changed = false;
    for (subject, entry) in quarantine {
        if let Some(existing) = records.get(subject) {
            if existing != entry {
                return Err(format!(
                    "conflicting existing valuation quarantine for {subject}"
                ));
            }
        } else {
            records.insert(subject.clone(), entry.clone());
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    if read()? != original {
        return Err("valuation quarantine changed during import".into());
    }
    create_dir_all(&graph_dir.join("values"))?;
    write_json(&path, &json).map_err(Into::into)
}

pub fn parse_legacy_valuations(
    rdf: &str,
    source_user: &str,
    source_graph: &str,
) -> Result<BTreeMap<String, LocalBlockValueRecord>, String> {
    validate_local_id(source_user, "source user")?;
    validate_local_id(source_graph, "source graph")?;
    let root = format!("urn:mnemosyne:user:{source_user}:graph:{source_graph}");
    let prefix = format!("{root}:valuation:");
    let mut subjects: BTreeMap<String, BTreeMap<String, Term>> = BTreeMap::new();
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(rdf.as_bytes()) {
        let quad = quad.map_err(|e| format!("invalid legacy valuation RDF: {e}"))?;
        if !matches!(&quad.graph_name, GraphName::NamedNode(g) if g.as_str() == root) {
            continue;
        }
        let NamedOrBlankNode::NamedNode(subject) = quad.subject else {
            continue;
        };
        if !subject.as_str().starts_with(&prefix) {
            continue;
        }
        if subject.as_str() == prefix {
            return Err("empty legacy valuation subject suffix".into());
        }
        let predicate = quad.predicate.as_str();
        let field = if predicate == RDF_TYPE {
            if !matches!(&quad.object, Term::NamedNode(n) if n.as_str() == format!("{DOC}BlockValuation"))
            {
                return Err(format!(
                    "unsupported valuation type on {subject}: {}",
                    quad.object
                ));
            }
            "type"
        } else {
            if predicate == format!("{DOC}type") {
                return Err(format!("unsupported valuation predicate {predicate}"));
            }
            predicate.strip_prefix(DOC).ok_or_else(|| {
                format!("unsupported valuation predicate {predicate} on {subject}")
            })?
        };
        if !matches!(
            field,
            "type"
                | "blockRef"
                | "rawImportanceSum"
                | "rawValenceSum"
                | "importanceCount"
                | "valenceCount"
                | "cumulativeImportance"
                | "cumulativeValence"
                | "lastValuatedAt"
                | "userImportance"
                | "userValence"
        ) {
            return Err(format!(
                "unsupported valuation field {predicate} on {subject}"
            ));
        }
        let fields = subjects.entry(subject.as_str().to_owned()).or_default();
        if let Some(previous) = fields.insert(field.to_owned(), quad.object.clone()) {
            if previous != quad.object {
                return Err(format!("conflicting duplicate {predicate} on {subject}"));
            }
        }
    }
    let mut records = BTreeMap::new();
    for (subject, fields) in subjects {
        let record = parse_record(&root, &fields).map_err(|e| format!("{subject}: {e}"))?;
        let key = block_value_key(&record.document_id, &record.block_id);
        if records.insert(key.clone(), record).is_some() {
            return Err(format!(
                "multiple legacy valuation subjects target block {key}"
            ));
        }
    }
    Ok(records)
}

fn literal<'a>(fields: &'a BTreeMap<String, Term>, field: &str) -> Result<Option<&'a str>, String> {
    match fields.get(field) {
        None => Ok(None),
        Some(Term::Literal(value)) if value.language().is_none() => Ok(Some(value.value())),
        _ => Err(format!("{field} must be a non-language literal")),
    }
}

fn number(fields: &BTreeMap<String, Term>, field: &str) -> Result<Option<f64>, String> {
    validate_numeric_datatype(fields, field)?;
    literal(fields, field)?
        .map(|text| {
            let value = text
                .parse::<f64>()
                .map_err(|_| format!("invalid number for {field}: {text}"))?;
            if !value.is_finite() {
                return Err(format!("nonfinite {field}"));
            }
            Ok(value)
        })
        .transpose()
}

fn count(fields: &BTreeMap<String, Term>, field: &str) -> Result<Option<usize>, String> {
    validate_numeric_datatype(fields, field)?;
    literal(fields, field)?
        .map(|text| {
            text.parse::<usize>()
                .map_err(|_| format!("invalid nonnegative integer for {field}: {text}"))
        })
        .transpose()
}

fn validate_numeric_datatype(fields: &BTreeMap<String, Term>, field: &str) -> Result<(), String> {
    if let Some(Term::Literal(value)) = fields.get(field) {
        let datatype = value.datatype();
        let kind = datatype
            .as_str()
            .strip_prefix("http://www.w3.org/2001/XMLSchema#");
        if !matches!(
            kind,
            Some(
                "string"
                    | "float"
                    | "double"
                    | "decimal"
                    | "integer"
                    | "nonNegativeInteger"
                    | "positiveInteger"
                    | "nonPositiveInteger"
                    | "negativeInteger"
                    | "long"
                    | "int"
                    | "short"
                    | "byte"
                    | "unsignedLong"
                    | "unsignedInt"
                    | "unsignedShort"
                    | "unsignedByte"
            )
        ) {
            return Err(format!(
                "unsupported numeric datatype for {field}: {datatype}"
            ));
        }
    }
    Ok(())
}

fn parse_record(
    root: &str,
    fields: &BTreeMap<String, Term>,
) -> Result<LocalBlockValueRecord, String> {
    let Some(Term::NamedNode(reference)) = fields.get("blockRef") else {
        return Err("missing or non-IRI blockRef".into());
    };
    let suffix = reference
        .as_str()
        .strip_prefix(&format!("{root}:doc:"))
        .ok_or("blockRef is outside the source root")?;
    let (document_id, block_id) = suffix.split_once("#block-").ok_or("malformed blockRef")?;
    validate_local_id(document_id, "document ID")?;
    validate_local_id(block_id, "block ID")?;
    let raw_importance_sum = number(fields, "rawImportanceSum")?;
    let raw_valence_sum = number(fields, "rawValenceSum")?;
    let importance_count = count(fields, "importanceCount")?;
    let valence_count = count(fields, "valenceCount")?;
    let cumulative_importance = number(fields, "cumulativeImportance")?;
    let cumulative_valence = number(fields, "cumulativeValence")?;
    if let Some(Term::Literal(timestamp)) = fields.get("lastValuatedAt") {
        if !matches!(
            timestamp.datatype().as_str(),
            "http://www.w3.org/2001/XMLSchema#string"
                | "http://www.w3.org/2001/XMLSchema#dateTime"
                | "http://www.w3.org/2001/XMLSchema#dateTimeStamp"
        ) {
            return Err("unsupported lastValuatedAt datatype".into());
        }
    }
    for (axis, raw, count, cumulative) in [
        (
            "importance",
            raw_importance_sum,
            importance_count,
            cumulative_importance,
        ),
        (
            "valence",
            raw_valence_sum,
            valence_count,
            cumulative_valence,
        ),
    ] {
        if cumulative.unwrap_or(0.0) != 0.0 && (raw.is_none() || count.is_none()) {
            return Err(format!(
                "missing raw sum/count with nonzero cumulative {axis}; reconstruction refused"
            ));
        }
        // Absence is allowed for manual-only rows, not partially populated aggregates.
        if (raw.is_some() || count.is_some() || cumulative.is_some())
            && !(raw.is_some() && count.is_some() && cumulative.is_some())
        {
            return Err(format!("incomplete {axis} aggregate statistics"));
        }
    }
    let record = LocalBlockValueRecord {
        valuation_receipts: Default::default(),
        document_id: document_id.into(),
        block_id: block_id.into(),
        raw_importance_sum: raw_importance_sum.unwrap_or(0.0),
        raw_valence_sum: raw_valence_sum.unwrap_or(0.0),
        importance_count: importance_count.unwrap_or(0),
        valence_count: valence_count.unwrap_or(0),
        cumulative_importance: cumulative_importance.unwrap_or(0.0),
        cumulative_valence: cumulative_valence.unwrap_or(0.0),
        valuation_count: importance_count
            .unwrap_or(0)
            .checked_add(valence_count.unwrap_or(0))
            .ok_or("valuation count overflow")?,
        last_valuated_at: literal(fields, "lastValuatedAt")?.unwrap_or("").into(),
        user_importance: number(fields, "userImportance")?,
        user_valence: number(fields, "userValence")?,
        user_rated_at: None,
        user_renewed_from: None,
        tags: Vec::new(),
    };
    validate_record(&record)?;
    Ok(record)
}

fn validate_record(r: &LocalBlockValueRecord) -> Result<(), String> {
    validate_local_id(&r.document_id, "document ID")?;
    validate_local_id(&r.block_id, "block ID")?;
    if [
        r.raw_importance_sum,
        r.raw_valence_sum,
        r.cumulative_importance,
        r.cumulative_valence,
    ]
    .into_iter()
    .chain(r.user_importance)
    .chain(r.user_valence)
    .any(|n| !n.is_finite())
    {
        return Err("nonfinite valuation number".into());
    }
    if r.raw_importance_sum < 0.0 || r.cumulative_importance < 0.0 {
        return Err("negative importance sum/cumulative importance".into());
    }
    if r.user_importance.is_some_and(|v| !(0.0..=5.0).contains(&v))
        || r.user_valence.is_some_and(|v| !(-5.0..=5.0).contains(&v))
    {
        return Err("manual valuation outside axis range".into());
    }
    if r.importance_count.checked_add(r.valence_count) != Some(r.valuation_count) {
        return Err("valuation_count must equal sum of axis counts".into());
    }
    if (r.cumulative_importance > 0.0 && (r.raw_importance_sum == 0.0 || r.importance_count == 0))
        || (r.cumulative_valence != 0.0 && (r.raw_valence_sum == 0.0 || r.valence_count == 0))
    {
        return Err(
            "native read compatibility backfill would change zero raw sum/count; import refused"
                .into(),
        );
    }
    if !r.last_valuated_at.is_empty() {
        chrono::DateTime::parse_from_rfc3339(&r.last_valuated_at)
            .map_err(|e| format!("invalid lastValuatedAt: {e}"))?;
    }
    Ok(())
}

/// Merge once, without normalizing existing configuration or attributing an observer.
/// The caller must exclude concurrent native writes for the entire operation.
pub fn install_legacy_valuations(
    graph_dir: &Path,
    graph_id: &str,
    records: &BTreeMap<String, LocalBlockValueRecord>,
) -> Result<(), String> {
    validate_local_id(graph_id, "graph ID")?;
    for (key, record) in records {
        validate_record(record)?;
        if *key != block_value_key(&record.document_id, &record.block_id) {
            return Err(format!("valuation record key mismatch: {key}"));
        }
    }
    if records.is_empty() {
        return Ok(());
    }
    let path = graph_dir.join("values/block-values.json");
    let original = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    // Keep the original JSON: typed read_value_store backfills records and normalizes config.
    let mut json: serde_json::Value = match &original {
        Some(bytes) => {
            serde_json::from_slice(bytes).map_err(|e| format!("invalid native value store: {e}"))?
        }
        None => serde_json::to_value(read_value_store(graph_dir, graph_id)?)
            .map_err(|e| e.to_string())?,
    };
    let existing: LocalValueStore = serde_json::from_value(json.clone())
        .map_err(|e| format!("invalid native value store: {e}"))?;
    if existing.graph_id != graph_id
        || !existing.observer.is_empty()
        || existing.schema_version != 1
    {
        return Err("existing value store is not this graph's supported shared commons".into());
    }
    let blocks = json
        .get_mut("blocks")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or("native value store blocks must be an object")?;
    let mut changed = false;
    for (key, record) in records {
        let incoming = serde_json::to_value(record).map_err(|e| e.to_string())?;
        if let Some(old) = existing.blocks.get(key) {
            if serde_json::to_value(old).map_err(|e| e.to_string())? != incoming {
                return Err(format!(
                    "conflicting existing valuation for {key}; refusing overwrite"
                ));
            }
        } else {
            blocks.insert(key.clone(), incoming);
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    // Catch intervening changes defensively; this is not a replacement for caller exclusion.
    let current = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    if current != original {
        return Err("native value store changed during import".into());
    }
    create_dir_all(&graph_dir.join("values"))?;
    write_json(&path, &json).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ROOT: &str = "urn:mnemosyne:user:alice:graph:source";

    fn quad(subject: &str, field: &str, object: &str, graph: &str) -> String {
        format!("<{subject}> <{DOC}{field}> {object} <{graph}> .\n")
    }

    fn row(name: &str, doc: &str, block: &str, fields: &[(&str, &str)]) -> String {
        let subject = format!("{ROOT}:valuation:{name}");
        let mut rdf = quad(
            &subject,
            "blockRef",
            &format!("<{ROOT}:doc:{doc}#block-{block}>"),
            ROOT,
        );
        for (field, value) in fields {
            rdf.push_str(&quad(&subject, field, &format!("\"{value}\""), ROOT));
        }
        rdf
    }

    fn aggregate() -> String {
        row(
            "opaque-subject",
            "doc.a",
            "block-a",
            &[
                ("rawImportanceSum", "7.125"),
                ("importanceCount", "3"),
                ("cumulativeImportance", "3.0224"),
                ("rawValenceSum", "-2.25"),
                ("valenceCount", "2"),
                ("cumulativeValence", "-1.7004"),
                ("userImportance", "0"),
                ("userValence", "0"),
                ("lastValuatedAt", "2026-09-09T14:00:00.123456789-07:00"),
            ],
        )
    }

    fn parse(rdf: &str) -> Result<BTreeMap<String, LocalBlockValueRecord>, String> {
        parse_legacy_valuations(rdf, "alice", "source")
    }

    #[test]
    fn planner_keeps_valid_rows_and_quarantines_missing_ref_losslessly() {
        let subject = format!("{ROOT}:valuation:orphan");
        let orphan = quad(
            &subject,
            "rawImportanceSum",
            "\"7.00\"^^<http://www.w3.org/2001/XMLSchema#float>",
            ROOT,
        ) + &quad(
            &subject,
            "importanceCount",
            "\"2\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            ROOT,
        ) + &quad(
            &subject,
            "lastValuatedAt",
            "\"2026-09-09T21:00:00.123456+00:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>",
            ROOT,
        ) + &quad(
            &subject,
            "unsupportedTag",
            "\"retain \\\"quote\\\" and \\n newline\"@en",
            ROOT,
        );
        let rdf = aggregate() + &orphan;
        assert!(parse(&rdf).is_err());
        let plan = plan_legacy_valuations(&rdf, "alice", "source").unwrap();
        assert_eq!(plan.records.len(), 1);
        assert!(plan.records.contains_key("doc.a:block-a"));
        assert_eq!(plan.quarantine.len(), 1);
        let entry = &plan.quarantine[&subject];
        assert!(entry["reason"].as_str().unwrap().contains("unsupported"));
        let semantics = |rdf: &str| {
            RdfParser::from_format(RdfFormat::NQuads)
                .for_slice(rdf.as_bytes())
                .map(|q| q.unwrap().to_string())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            semantics(&orphan),
            semantics(entry["rdf"].as_str().unwrap())
        );
        // A plain missing-ref row is quarantined for precisely that reason.
        let missing = quad(&subject, "importanceCount", "\"2\"", ROOT);
        let plan = plan_legacy_valuations(&(aggregate() + &missing), "alice", "source").unwrap();
        assert!(plan.quarantine[&subject]["reason"]
            .as_str()
            .unwrap()
            .contains("blockRef"));
        assert_eq!(plan.records.len(), 1);
        assert!(plan_legacy_valuations(&(rdf + "broken RDF"), "alice", "source").is_err());
        assert!(plan_legacy_valuations("", "../alice", "source").is_err());
    }

    #[test]
    fn planner_quarantines_all_duplicate_targets_and_is_order_independent() {
        let first = row("one", "d", "b", &[("userImportance", "0")]);
        let second = row("two", "d", "b", &[("userImportance", "1")]);
        let third = row("three", "d", "b", &[("unsupportedTag", "retained")]);
        let rdf = aggregate() + &first + &second + &third;
        let plan = plan_legacy_valuations(&rdf, "alice", "source").unwrap();
        assert_eq!(plan.records.len(), 1);
        assert!(!plan.records.contains_key("d:b"));
        assert_eq!(plan.quarantine.len(), 3);
        for name in ["one", "two", "three"] {
            assert!(
                plan.quarantine[&format!("{ROOT}:valuation:{name}")]["reason"]
                    .as_str()
                    .unwrap()
                    .contains("multiple legacy")
            );
        }
        let reordered = rdf.lines().rev().collect::<Vec<_>>().join("\n");
        let retry = plan_legacy_valuations(&reordered, "alice", "source").unwrap();
        assert_eq!(plan.quarantine, retry.quarantine);
        // Invalid rows with an explicit target still prevent its valid counterpart from installing.
        let plan = plan_legacy_valuations(&(first + &third), "alice", "source").unwrap();
        assert!(plan.records.is_empty());
        assert_eq!(plan.quarantine.len(), 2);
    }

    #[test]
    fn quarantine_install_merges_retries_and_rejects_conflicts_without_writes() {
        let dir = std::env::temp_dir().join(format!("legacy-quarantine-{}", uuid::Uuid::new_v4()));
        let subject = format!("{ROOT}:valuation:orphan");
        let plan = plan_legacy_valuations(
            &quad(&subject, "importanceCount", "\"2\"", ROOT),
            "alice",
            "source",
        )
        .unwrap();
        install_quarantine(&dir, &plan.quarantine).unwrap();
        let path = dir.join("values/legacy-valuation-quarantine.json");
        let first = fs::read(&path).unwrap();
        let json: Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(
            json,
            json!({"schemaVersion": 1, "records": plan.quarantine})
        );
        install_quarantine(&dir, &plan.quarantine).unwrap();
        assert_eq!(fs::read(&path).unwrap(), first);
        let other = format!("{ROOT}:valuation:another");
        let additional = plan_legacy_valuations(
            &quad(&other, "importanceCount", "\"3\"", ROOT),
            "alice",
            "source",
        )
        .unwrap();
        install_quarantine(&dir, &additional.quarantine).unwrap();
        let merged = fs::read(&path).unwrap();
        let json: Value = serde_json::from_slice(&merged).unwrap();
        assert_eq!(json["records"].as_object().unwrap().len(), 2);
        assert_eq!(json["records"][&subject], plan.quarantine[&subject]);
        let mut conflict = plan.quarantine.clone();
        conflict.get_mut(&subject).unwrap()["reason"] = json!("different");
        conflict.insert(
            format!("{ROOT}:valuation:aaa"),
            json!({"rdf": "", "reason": "new"}),
        );
        assert!(install_quarantine(&dir, &conflict)
            .unwrap_err()
            .contains("conflicting existing"));
        assert_eq!(fs::read(&path).unwrap(), merged);
        assert!(!dir.join("values/block-values.json").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preserves_all_fields_and_exact_timestamp() {
        let records = parse(&aggregate()).unwrap();
        assert_eq!(
            serde_json::to_value(&records["doc.a:block-a"]).unwrap(),
            json!({
                "documentId": "doc.a", "blockId": "block-a", "rawImportanceSum": 7.125,
                "rawValenceSum": -2.25, "importanceCount": 3, "valenceCount": 2,
                "valuationCount": 5, "cumulativeImportance": 3.0224, "cumulativeValence": -1.7004,
                "userImportance": 0.0, "userValence": 0.0, "tags": [],
                "lastValuatedAt": "2026-09-09T14:00:00.123456789-07:00"
            })
        );
    }

    #[test]
    fn manual_only_zero_and_absent_timestamp() {
        let records = parse(&row(
            "manual",
            "d",
            "b",
            &[("userImportance", "0"), ("userValence", "0")],
        ))
        .unwrap();
        let r = &records["d:b"];
        assert_eq!(r.user_importance, Some(0.0));
        assert_eq!(r.user_valence, Some(0.0));
        assert_eq!(
            (r.valuation_count, r.importance_count, r.valence_count),
            (0, 0, 0)
        );
        assert_eq!(
            (
                r.raw_importance_sum,
                r.raw_valence_sum,
                r.cumulative_importance,
                r.cumulative_valence
            ),
            (0.0, 0.0, 0.0, 0.0)
        );
        assert!(r.last_valuated_at.is_empty());
    }

    #[test]
    fn mixed_graphs_only_exact_named_root_and_subject_are_selected() {
        let mut rdf = aggregate();
        let subject = format!("{ROOT}:valuation:ignored");
        for graph in [
            format!("{ROOT}:projection:salience"),
            format!("{ROOT}-other"),
            ROOT.replace("alice", "bob"),
        ] {
            rdf.push_str(&quad(&subject, "unsupported", "\"not imported\"", &graph));
        }
        rdf.push_str(&format!(
            "<{subject}> <{DOC}unsupported> \"default graph\" .\n"
        ));
        rdf.push_str(&quad(
            "urn:elsewhere:valuation:x",
            "unsupported",
            "\"foreign subject\"",
            ROOT,
        ));
        assert_eq!(parse(&rdf).unwrap().len(), 1);
    }

    #[test]
    fn rejects_conflicting_predicates_and_multiple_subjects_for_one_block() {
        let base = row("one", "d", "b", &[("userImportance", "0")]);
        let duplicate = quad(
            &format!("{ROOT}:valuation:one"),
            "userImportance",
            "\"0\"",
            ROOT,
        );
        assert!(parse(&(base.clone() + &duplicate)).is_ok());
        let conflict = quad(
            &format!("{ROOT}:valuation:one"),
            "userImportance",
            "\"1\"",
            ROOT,
        );
        assert!(parse(&(base.clone() + &conflict))
            .unwrap_err()
            .contains("conflicting duplicate"));
        assert!(
            parse(&(base + &row("two", "d", "b", &[("userImportance", "0")])))
                .unwrap_err()
                .contains("multiple legacy")
        );
    }

    #[test]
    fn malformed_data_is_loud() {
        for (field, value) in [
            ("rawImportanceSum", "-1"),
            ("importanceCount", "-1"),
            ("valenceCount", "1.5"),
            ("userImportance", "NaN"),
            ("userValence", "inf"),
            ("userImportance", "6"),
            ("lastValuatedAt", "yesterday"),
            ("tags", "important"),
            ("cumulativeValence", "-2"),
            ("importanceCount", "999999999999999999999999999999"),
        ] {
            assert!(
                parse(&row("bad", "d", "b", &[(field, value)])).is_err(),
                "{field}={value}"
            );
        }
        for (doc, block) in [("..", "b"), ("d", ""), ("d", "b/escape"), ("d", "b#extra")] {
            assert!(parse(&row("bad", doc, block, &[])).is_err());
        }
        let subject = format!("{ROOT}:valuation:bad");
        assert!(parse(&quad(
            &subject,
            "blockRef",
            "<urn:other:doc:d#block-b>",
            ROOT
        ))
        .is_err());
        assert!(parse(&quad(&subject, "userImportance", "\"0\"", ROOT)).is_err());
        assert!(parse("this is not RDF").is_err());
        assert!(parse(
            &(row("bad", "d", "b", &[])
                + &quad(
                    &subject,
                    "userImportance",
                    "\"1\"^^<http://www.w3.org/2001/XMLSchema#boolean>",
                    ROOT
                ))
        )
        .is_err());
    }

    #[test]
    fn rejects_data_native_reader_would_reconstruct() {
        for (raw, count) in [("0", "2"), ("4", "0")] {
            let rdf = row(
                "bad",
                "d",
                "b",
                &[
                    ("rawImportanceSum", raw),
                    ("importanceCount", count),
                    ("cumulativeImportance", "2"),
                ],
            );
            assert!(parse(&rdf).unwrap_err().contains("backfill"));
        }
        let missing = row("bad", "d", "b", &[("cumulativeImportance", "2")]);
        assert!(parse(&missing)
            .unwrap_err()
            .contains("reconstruction refused"));
    }

    #[test]
    fn install_roundtrip_retry_conflict_and_independent_stores() {
        let dir = std::env::temp_dir().join(format!("legacy-valuations-{}", uuid::Uuid::new_v4()));
        let records =
            parse(&(aggregate() + &row("manual", "d", "b", &[("userImportance", "0")]))).unwrap();
        install_legacy_valuations(&dir, "native", &records).unwrap();
        let path = dir.join("values/block-values.json");
        let first = fs::read(&path).unwrap();
        let store = read_value_store(&dir, "native").unwrap();
        assert_eq!(
            serde_json::to_value(store.blocks).unwrap(),
            serde_json::to_value(&records).unwrap()
        );
        install_legacy_valuations(&dir, "native", &records).unwrap();
        assert_eq!(fs::read(&path).unwrap(), first);

        let mut native: serde_json::Value = serde_json::from_slice(&first).unwrap();
        native["config"]["futureSetting"] = json!({"keep": true});
        native["futureStoreMetadata"] = json!("retain");
        write_json(&path, &native).unwrap();
        let observer = dir.join("values/alpha/block-values.json");
        fs::create_dir_all(observer.parent().unwrap()).unwrap();
        fs::write(&observer, b"independent observer bytes").unwrap();
        let additional = parse(&row("new", "new", "b", &[("userValence", "-1")])).unwrap();
        install_legacy_valuations(&dir, "native", &additional).unwrap();
        let merged: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(merged["config"], native["config"]);
        assert_eq!(merged["configHistory"], native["configHistory"]);
        assert_eq!(merged["futureStoreMetadata"], native["futureStoreMetadata"]);
        assert_eq!(fs::read(&observer).unwrap(), b"independent observer bytes");

        let before_conflict = fs::read(&path).unwrap();
        let mut conflicting = records.clone();
        conflicting
            .get_mut("doc.a:block-a")
            .unwrap()
            .user_importance = Some(4.0);
        conflicting.extend(parse(&row("earlier", "aaa", "b", &[])).unwrap());
        assert!(install_legacy_valuations(&dir, "native", &conflicting)
            .unwrap_err()
            .contains("conflicting existing"));
        assert_eq!(
            fs::read(&path).unwrap(),
            before_conflict,
            "conflict must not partially install"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
