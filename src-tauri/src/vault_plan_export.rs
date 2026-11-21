//! Reads the narrow YAML shape emitted by our version-1 Markdown exporter.
//! Unsupported shapes stay raw and are reported, never guessed into wire claims.
use super::*;

#[derive(Debug, Serialize)]
pub(crate) struct ExportWire {
    pub source: usize,
    pub target: usize,
    pub predicate: String,
    /// Historical IDs only: these are not new document block IDs.
    pub original_source_block: Option<String>,
    pub original_target_block: Option<String>,
}

fn read_verified(
    entry: &Input,
    read: &mut impl FnMut(usize) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    let bytes = read(entry.index)?;
    if bytes.len() as u64 != entry.bytes || format!("{:x}", Sha256::digest(&bytes)) != entry.sha256
    {
        return Err("captured export metadata integrity mismatch".into());
    }
    Ok(bytes)
}

fn quoted(value: &str) -> Result<String, String> {
    let value = value.trim();
    if !value.starts_with(['\'', '"']) {
        return Err("export field is not a quoted string".into());
    }
    scalar(value)
        .filter(|v| !v.is_empty() && !v.chars().any(char::is_control))
        .ok_or_else(|| "invalid exported string".into())
}

struct Header {
    id: String,
    wires: Vec<BTreeMap<String, String>>,
}

fn header(raw: &str) -> Result<Header, String> {
    let mut id = None;
    let mut wires = vec![];
    let mut current: Option<BTreeMap<String, String>> = None;
    let mut in_wires = false;
    let mut saw_wires = false;
    for line in raw.lines().skip(1) {
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("id: ") {
            if id.replace(quoted(value)?).is_some() {
                return Err("duplicate exported id".into());
            }
            in_wires = false;
        } else if line == "wires:" {
            if saw_wires {
                return Err("duplicate exported wires field".into());
            }
            saw_wires = true;
            in_wires = true;
        } else if in_wires {
            if let Some(value) = line.strip_prefix("  - predicate: ") {
                if let Some(row) = current.take() {
                    wires.push(row);
                }
                current = Some(BTreeMap::from([("predicate".into(), quoted(value)?)]));
            } else if let Some(value) = line.strip_prefix("    ") {
                let (key, value) = value.split_once(": ").ok_or("unsupported wire field")?;
                if !matches!(
                    key,
                    "to" | "toId" | "from" | "fromId" | "block" | "toBlock" | "fromBlock"
                ) {
                    return Err("unsupported wire key".into());
                }
                let row = current.as_mut().ok_or("wire field without predicate")?;
                if row.insert(key.into(), quoted(value)?).is_some() {
                    return Err("duplicate wire field".into());
                }
            } else if !line.starts_with(char::is_whitespace) {
                in_wires = false;
            } else if !line.trim().is_empty() {
                return Err("unsupported wire indentation".into());
            }
        }
    }
    if let Some(row) = current {
        wires.push(row);
    }
    for row in &wires {
        let outgoing = row.contains_key("toId");
        if outgoing == row.contains_key("fromId")
            || !row.contains_key(if outgoing { "to" } else { "from" })
        {
            return Err("ambiguous exported wire direction".into());
        }
        if row.contains_key(if outgoing { "from" } else { "to" })
            || row.contains_key(if outgoing { "fromBlock" } else { "toBlock" })
        {
            return Err("mixed exported wire direction".into());
        }
    }
    Ok(Header {
        id: id.ok_or("missing exported id")?,
        wires,
    })
}

pub(super) fn recognized(
    entries: &[Input],
    read: &mut impl FnMut(usize) -> Result<Vec<u8>, String>,
) -> Result<bool, String> {
    let Some(manifest) = entries
        .iter()
        .find(|e| e.path == "graph.json" && e.kind == "attachment")
    else {
        return Ok(false);
    };
    if manifest.bytes > MAX_FRONT_MATTER as u64 {
        return Err("graph.json exceeds bounded export recognition size".into());
    }
    let bytes = read_verified(manifest, read)?;
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(false);
    };
    if !matches!(
        value["format"].as_str(),
        Some("mycl-markdown-folder" | "mycyl-markdown-folder")
    ) {
        return Ok(false);
    }
    if value["version"].as_u64() != Some(1) {
        return Err("unsupported Markdown-folder export version".into());
    }
    Ok(true)
}

pub(super) fn augment(
    plan: &mut Plan,
    entries: &[Input],
    read: &mut impl FnMut(usize) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    let mut headers = BTreeMap::new();
    let mut ids = BTreeMap::<String, Vec<usize>>::new();
    for doc in &plan.documents {
        let Some(raw) = &doc.metadata.front_matter else {
            continue;
        };
        match header(raw) {
            Ok(header) => {
                ids.entry(header.id.clone()).or_default().push(doc.entry);
                headers.insert(doc.entry, header);
            }
            Err(message) => issue(
                &mut plan.issues,
                &doc.path,
                "unmapped_export_metadata",
                message,
            ),
        }
    }
    let mut seen = BTreeSet::new();
    let entries: BTreeMap<_, _> = entries.iter().map(|e| (e.index, e)).collect();
    let document_paths: BTreeMap<_, _> = plan
        .documents
        .iter()
        .map(|d| (d.entry, d.path.clone()))
        .collect();
    for doc in &mut plan.documents {
        let Some(header) = headers.get(&doc.entry) else {
            continue;
        };
        if ids[&header.id].len() != 1 {
            issue(
                &mut plan.issues,
                &doc.path,
                "ambiguous_export_id",
                "Duplicate exported document identity; typed wires not inferred",
            );
            continue;
        }
        for row in &header.wires {
            let outgoing = row.contains_key("toId");
            let other = &row[if outgoing { "toId" } else { "fromId" }];
            let Some(targets) = ids.get(other).filter(|v| v.len() == 1) else {
                issue(
                    &mut plan.issues,
                    &doc.path,
                    "unresolved_export_wire",
                    format!("Missing or ambiguous exported target {other}"),
                );
                continue;
            };
            let (source, target) = if outgoing {
                (doc.entry, targets[0])
            } else {
                (targets[0], doc.entry)
            };
            let own = row.get("block").cloned();
            let other = row
                .get(if outgoing { "toBlock" } else { "fromBlock" })
                .cloned();
            let (source_block, target_block) = if outgoing { (own, other) } else { (other, own) };
            let predicate = row["predicate"].clone();
            if !seen.insert((
                source,
                target,
                predicate.clone(),
                source_block.clone(),
                target_block.clone(),
            )) {
                continue;
            }
            if plan.export_wires.len() >= MAX_LINKS {
                return Err("too many exported wires".into());
            }
            if source_block.is_some() || target_block.is_some() {
                issue(
                    &mut plan.issues,
                    &doc.path,
                    "unmapped_export_block",
                    "Original block endpoints retained; no new block identity inferred",
                );
            }
            plan.export_wires.push(ExportWire {
                source,
                target,
                predicate,
                original_source_block: source_block,
                original_target_block: target_block,
            });
        }
        // Exact equality recognizes the v1 convention, not authorship. Report
        // the omitted range: a person could have written identical text.
        let mut footer = String::new();
        let mut complete = true;
        for row in header.wires.iter().filter(|r| r.contains_key("toId")) {
            let Some(targets) = ids.get(&row["toId"]).filter(|v| v.len() == 1) else {
                complete = false;
                break;
            };
            footer.push_str(&format!(
                "- {} {}\n",
                row["predicate"],
                crate::markdown_folder_export::wikilink(
                    stem(&document_paths[&targets[0]]),
                    &row["to"]
                )
            ));
        }
        if complete && !footer.is_empty() {
            let entry = entries.get(&doc.entry).ok_or("missing export entry")?;
            let bytes = read_verified(entry, read)?;
            let source = std::str::from_utf8(&bytes).map_err(|_| "export note changed encoding")?;
            let suffix = format!("\n## Wires\n\n{footer}");
            if source.ends_with(&suffix) && source.len() - suffix.len() >= doc.metadata.body_start {
                doc.body_end = source.len() - suffix.len();
                issue(&mut plan.issues, &doc.path, "export_footer_omitted", format!(
                    "Version-1 wire footer convention: omit body bytes {}..{}; identical authored text is indistinguishable. Original captured bytes must be retained and this omission shown before commit",
                    doc.body_end, source.len()
                ));
            }
        }
    }
    let ends: BTreeMap<_, _> = plan
        .documents
        .iter()
        .map(|d| (d.entry, d.body_end))
        .collect();
    plan.links.retain(|link| {
        ends.get(&link.source)
            .is_some_and(|end| link.source_offset < *end)
    });
    Ok(())
}
