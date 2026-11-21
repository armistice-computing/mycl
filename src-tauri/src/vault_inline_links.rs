//! Pair resolved source occurrences with native inline atoms and their wires.
//! Source bytes remain unchanged; parser-attested ranges avoid code/text guessing.
use crate::{
    vault_note::{PreparedNote, PARSER_VERSION},
    vault_plan::Plan,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Destination {
    pub graph_id: String,
    pub incarnation: String,
    pub operation_id: String,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct InlineWire {
    pub id: String,
    /// Matches the active frontend's DEFAULT_WIKILINK_PREDICATE.
    pub predicate: String,
    pub source_offset: usize,
    pub source_block_id: String,
    pub target_entry: Option<usize>,
    pub target_graph: String,
    pub target_id: String,
    pub target_block_id: Option<String>,
    pub artifact: bool,
}
struct Occurrence {
    start: usize,
    end: usize,
    raw: String,
    label: String,
    embedded: bool,
    formatted: bool,
}
struct Replacement {
    start: usize,
    end: usize,
    token: String,
    atom: Value,
    wire: InlineWire,
}

#[derive(Default)]
struct Warnings {
    items: Vec<String>,
    omitted: usize,
}
impl Warnings {
    fn push(&mut self, message: String) {
        if self.items.len() < 128 {
            self.items.push(message);
        } else {
            self.omitted += 1;
        }
    }
    fn is_empty(&self) -> bool {
        self.items.is_empty() && self.omitted == 0
    }
    fn finish(mut self) -> Vec<String> {
        if self.omitted > 0 {
            self.items.push(format!(
                "{} additional inline-conversion warnings omitted; original source retained",
                self.omitted
            ));
        }
        self.items
    }
}

fn occurrences(body: &str) -> Result<Vec<Occurrence>, String> {
    let mut stack: Vec<Occurrence> = vec![];
    let mut result = vec![];
    for (event, range) in Parser::new_ext(
        body,
        crate::crdt_engine::content_parse::md_options() | Options::ENABLE_WIKILINKS,
    )
    .into_offset_iter()
    {
        match event {
            Event::Start(Tag::Link {
                dest_url, title, ..
            }) => stack.push(Occurrence {
                start: range.start,
                end: range.end,
                raw: dest_url.to_string(),
                label: String::new(),
                embedded: false,
                formatted: !title.is_empty(),
            }),
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => stack.push(Occurrence {
                start: range.start,
                end: range.end,
                raw: dest_url.to_string(),
                label: String::new(),
                embedded: true,
                formatted: !title.is_empty(),
            }),
            Event::End(TagEnd::Link | TagEnd::Image) => {
                if let Some(item) = stack.pop() {
                    result.push(item);
                }
            }
            Event::Text(text) => {
                for item in &mut stack {
                    item.label.push_str(&text);
                }
            }
            Event::Code(text) => {
                for item in &mut stack {
                    item.label.push_str(&text);
                    item.formatted = true;
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                for item in &mut stack {
                    item.label.push(' ');
                }
            }
            Event::Start(_) | Event::InlineHtml(_) => {
                for item in &mut stack {
                    item.formatted = true;
                }
            }
            _ => {}
        }
        if result.len() + stack.len() > 100_000 || stack.iter().any(|o| o.label.len() > 4096) {
            return Err("inline reference planning bound exceeded".into());
        }
    }
    result.sort_by_key(|o| (o.start, o.end));
    Ok(result)
}
fn contains(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(s) => s.contains(needle),
        Value::Array(a) => a.iter().any(|v| contains(v, needle)),
        Value::Object(m) => m.values().any(|v| contains(v, needle)),
        _ => false,
    }
}

pub(crate) struct Resolver<'a> {
    sources: BTreeMap<&'a str, usize>,
    links: BTreeMap<usize, BTreeMap<usize, &'a crate::vault_plan::Link>>,
    docs: BTreeMap<usize, &'a str>,
    entries: BTreeMap<usize, &'a crate::vault_plan::Input>,
    tags: BTreeMap<usize, BTreeMap<usize, &'a crate::vault_plan::tags::Reference>>,
    anchors: BTreeMap<usize, crate::vault_note::fragments::Anchors>,
}
impl<'a> Resolver<'a> {
    pub(crate) fn new(plan: &'a Plan) -> Self {
        let mut links: BTreeMap<usize, BTreeMap<usize, &crate::vault_plan::Link>> = BTreeMap::new();
        for link in &plan.links {
            links
                .entry(link.source)
                .or_default()
                .insert(link.source_offset, link);
        }
        let mut tags: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
        for tag in &plan.tags.references {
            if let Some((start, _)) = tag.range {
                tags.entry(tag.source).or_default().insert(start, tag);
            }
        }
        Self {
            anchors: BTreeMap::new(),
            tags,
            sources: plan
                .documents
                .iter()
                .map(|d| (d.id.as_str(), d.entry))
                .collect(),
            links,
            docs: plan
                .documents
                .iter()
                .map(|d| (d.entry, d.id.as_str()))
                .collect(),
            entries: plan.capture_entries.iter().map(|e| (e.index, e)).collect(),
        }
    }
    pub(crate) fn install(
        &self,
        note: &mut PreparedNote,
        destination: &Destination,
    ) -> Result<(), String> {
        install_indexed(self, note, destination)
    }
    pub(crate) fn register_note(&mut self, note: &PreparedNote) -> Result<(), String> {
        note.verify()?;
        let source = self
            .sources
            .get(note.document_id.as_str())
            .ok_or("anchor source not planned")?;
        if self.anchors.insert(*source, note.anchors.clone()).is_some() {
            return Err("duplicate anchor source".into());
        }
        Ok(())
    }
}
#[cfg(test)]
fn install(plan: &Plan, note: &mut PreparedNote, destination: &Destination) -> Result<(), String> {
    Resolver::new(plan).install(note, destination)
}

fn install_indexed(
    index: &Resolver<'_>,
    note: &mut PreparedNote,
    destination: &Destination,
) -> Result<(), String> {
    note.verify()?;
    let source = index
        .sources
        .get(note.document_id.as_str())
        .ok_or("inline source not planned")?;
    let empty_links = BTreeMap::new();
    let links = index.links.get(source).unwrap_or(&empty_links);
    let empty_tags = BTreeMap::new();
    let tags = index.tags.get(source).unwrap_or(&empty_tags);
    let body = std::str::from_utf8(&note.original)
        .map_err(|_| "invalid inline source")?
        .get(note.body_start..note.body_end)
        .ok_or("invalid inline source range")?;
    let mut parsed = occurrences(body)?;
    for (start, end, name) in crate::vault_plan::tags::inline(body) {
        parsed.push(Occurrence {
            start,
            end,
            raw: name.clone(),
            label: format!("#{name}"),
            embedded: false,
            formatted: false,
        });
    }
    parsed.sort_by_key(|item| item.start);
    let docs = &index.docs;
    let entries = &index.entries;
    let mut prefix = None;
    for salt in 0..16 {
        let candidate = format!(
            "MYCL_IMPORT_REF_{:x}X",
            Sha256::digest(format!(
                "{}:{}:{salt}",
                note.document_id, note.source_sha256
            ))
        );
        if !body.contains(&candidate) && !contains(&note.tiptap_json, &candidate) {
            prefix = Some(candidate);
            break;
        }
    }
    let prefix = prefix.ok_or("cannot reserve inline placeholder namespace")?;
    let mut replacements = vec![];
    let mut warnings = Warnings::default();
    let mut furthest_end = 0;
    for (ordinal, item) in parsed.iter().enumerate() {
        let overlaps = item.start < furthest_end
            || parsed
                .get(ordinal + 1)
                .is_some_and(|next| next.start < item.end);
        furthest_end = furthest_end.max(item.end);
        let offset = note.body_start + item.start;
        let (target, artifact, target_id, predicate, target_block_id) = if let Some(tag) =
            tags.get(&offset)
        {
            if tag.range != Some((offset, note.body_start + item.end)) || tag.name != item.raw {
                return Err("inline tag differs from reviewed occurrence".into());
            }
            (
                None,
                false,
                tag.target_id.clone(),
                crate::vault_plan::tags::PREDICATE,
                None,
            )
        } else {
            let Some(link) = links.get(&offset) else {
                continue;
            };
            let Some(target) = link.target else {
                continue;
            };
            if item.embedded || link.embedded {
                continue;
            }
            let expected_raw = match &link.fragment {
                Some(fragment) => format!("{}#{fragment}", link.raw),
                None => link.raw.clone(),
            };
            if item.raw != expected_raw {
                return Err("inline occurrence differs from reviewed link".into());
            }
            let artifact = link.target_kind.as_deref() == Some("artifact");
            let target_block_id = if let Some(fragment) = &link.fragment {
                let resolved = if artifact {
                    None
                } else {
                    index
                        .anchors
                        .get(&target)
                        .and_then(|a| a.resolve(fragment, link.wiki))
                };
                let Some(id) = resolved else {
                    warnings.push(format!("Reference at byte {offset} retained: fragment has no unique supported target block"));
                    continue;
                };
                Some(id)
            } else {
                None
            };
            let target_id = if artifact {
                crate::vault_graph_stage::artifact_id(
                    &destination.graph_id,
                    &destination.incarnation,
                    &destination.operation_id,
                    entries.get(&target).ok_or("missing inline file")?,
                )?
            } else {
                docs.get(&target)
                    .ok_or("missing inline document")?
                    .to_string()
            };
            (
                Some(target),
                artifact,
                target_id,
                "relatedTo",
                target_block_id,
            )
        };
        // Nested links/images cannot be replaced independently without losing
        // the enclosing occurrence. Retain their original rendering explicitly.
        if overlaps || item.label.is_empty() {
            warnings.push(format!("Reference at byte {offset} retained without inline conversion (nested or empty label)"));
            continue;
        }
        let id = format!(
            "vault-inline-{:x}",
            Sha256::digest(
                serde_json::to_vec(&(&note.document_id, offset, &destination.graph_id, &target_id))
                    .map_err(|e| e.to_string())?
            )
        );
        let token = format!("{prefix}{ordinal}Z");
        let atom = json!({"type":"wikilink","attrs":{
            "targetDocId":if artifact{""}else{target_id.as_str()},"targetGraphId":destination.graph_id,
            "targetBlockId":target_block_id,"targetArtifactId":if artifact{Some(target_id.as_str())}else{None},
            "label":item.label,"blockPreview":null,"wireId":id,"filePath":null,"region":null
        }});
        if item.formatted {
            warnings.push(format!("Reference label/title at byte {offset} uses native plain chip styling; original formatting retained in source"));
        }
        replacements.push(Replacement {
            start: item.start,
            end: item.end,
            token,
            atom,
            wire: InlineWire {
                id,
                predicate: predicate.into(),
                source_offset: offset,
                source_block_id: String::new(),
                target_entry: target,
                target_graph: destination.graph_id.clone(),
                target_id,
                target_block_id,
                artifact,
            },
        });
    }
    if replacements.is_empty() {
        if !warnings.is_empty() {
            note.install_inline_links(note.tiptap_json.clone(), vec![], warnings.finish())?;
        }
        return Ok(());
    }
    let rewritten = crate::vault_note::images::rewrite(
        body,
        &note.images,
        replacements
            .iter()
            .map(|r| (r.start, r.end, r.token.clone())),
    )?;
    let parsed = crate::crdt_engine::content_parse::parse_write_content_for_operation(
        &rewritten,
        Some("markdown"),
        &format!("vault-note-v{PARSER_VERSION}:{}", note.document_id),
    )?;
    let mut content = parsed.tiptap_json;
    for warning in parsed.warnings {
        warnings.push(warning);
    }
    let by_token: BTreeMap<_, _> = replacements.iter().map(|r| (r.token.as_str(), r)).collect();
    let mut used = BTreeSet::new();
    replace(&mut content, &prefix, &by_token, &mut used, &mut warnings)?;
    if used.len() != replacements.len() || contains(&content, &prefix) {
        return Err("inline references did not survive Markdown preparation".into());
    }
    // The projection is the authority for addressable blocks: nested list
    // paragraphs must not be guessed to be independent source blocks.
    let projection =
        crate::crdt_engine::projection::materialize_tiptap_json(&content, &note.document_id);
    let original_projection = crate::crdt_engine::projection::materialize_tiptap_json(
        &note.tiptap_json,
        &note.document_id,
    );
    let shape = |blocks: &Value| -> Vec<Value> {
        blocks
            .as_array()
            .into_iter()
            .flatten()
            .map(|b| {
                json!([
                    b["id"],
                    b["type"],
                    b["parentId"],
                    b["level"],
                    b["checked"],
                    b["language"]
                ])
            })
            .collect()
    };
    if shape(&projection.blocks_json) != shape(&original_projection.blocks_json) {
        return Err("inline conversion would change document block structure".into());
    }
    let wire_ids: BTreeSet<_> = replacements.iter().map(|r| r.wire.id.as_str()).collect();
    let mut blocks = BTreeMap::new();
    for block in projection
        .blocks_json
        .as_array()
        .ok_or("missing projected blocks")?
    {
        for mark in block["marks"].as_array().into_iter().flatten() {
            if let Some(wire) = mark["wireId"].as_str() {
                if wire_ids.contains(wire)
                    && blocks
                        .insert(
                            wire.to_string(),
                            block["id"]
                                .as_str()
                                .ok_or("missing source block id")?
                                .to_string(),
                        )
                        .is_some()
                {
                    return Err("inline wire has multiple projected source occurrences".into());
                }
            }
        }
    }
    let wires = replacements
        .into_iter()
        .map(|mut r| {
            r.wire.source_block_id = blocks
                .remove(&r.wire.id)
                .ok_or("inline wire has no projected source block")?;
            Ok(r.wire)
        })
        .collect::<Result<Vec<_>, String>>()?;
    note.install_inline_links(content, wires, warnings.finish())
}

#[cfg(test)]
#[path = "vault_inline_links_tests.rs"]
mod tests;

fn replace(
    node: &mut Value,
    prefix: &str,
    items: &BTreeMap<&str, &Replacement>,
    used: &mut BTreeSet<String>,
    warnings: &mut Warnings,
) -> Result<(), String> {
    let Some(children) = node.get_mut("content").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let mut output = vec![];
    for mut child in std::mem::take(children) {
        if child["type"] != "text" {
            replace(&mut child, prefix, items, used, warnings)?;
            output.push(child);
            continue;
        }
        let text = child["text"].as_str().unwrap_or("").to_string();
        let mut at = 0;
        while let Some(start) = text[at..].find(prefix).map(|p| p + at) {
            if start > at {
                let mut part = child.clone();
                part["text"] = json!(&text[at..start]);
                output.push(part);
            }
            let end = text[start + prefix.len()..]
                .find('Z')
                .map(|p| start + prefix.len() + p + 1)
                .ok_or("broken inline placeholder")?;
            let token = &text[start..end];
            let item = items.get(token).ok_or("unknown inline placeholder")?;
            if !used.insert(token.into()) {
                return Err("duplicated inline placeholder".into());
            }
            if child["marks"].as_array().is_some_and(|m| !m.is_empty()) {
                warnings.push(format!("Reference at byte {} uses native chip styling; surrounding source formatting retained in original",item.wire.source_offset));
            }
            output.push(item.atom.clone());
            at = end;
        }
        if at < text.len() {
            child["text"] = json!(&text[at..]);
            output.push(child);
        }
    }
    *children = output;
    Ok(())
}

pub(crate) fn validate_projection(
    content: &Value,
    blocks: &Value,
    wires: &[InlineWire],
) -> Result<(), String> {
    if wires.is_empty() {
        return Ok(());
    }
    let expected: BTreeMap<_, _> = wires.iter().map(|w| (w.id.as_str(), w)).collect();
    if expected.len() != wires.len() {
        return Err("duplicate inline wire identity".into());
    }
    fn visit(
        node: &Value,
        expected: &BTreeMap<&str, &InlineWire>,
        seen: &mut BTreeSet<String>,
    ) -> Result<(), String> {
        if node["type"] == "wikilink" {
            if let Some(wire) = node["attrs"]["wireId"]
                .as_str()
                .and_then(|id| expected.get(id))
            {
                let attrs = &node["attrs"];
                if !seen.insert(wire.id.clone())
                    || attrs["targetGraphId"] != wire.target_graph
                    || attrs["targetDocId"]
                        != if wire.artifact {
                            ""
                        } else {
                            wire.target_id.as_str()
                        }
                    || (wire.artifact && attrs["targetArtifactId"] != wire.target_id)
                    || (!wire.artifact && !attrs["targetArtifactId"].is_null())
                    || attrs["targetBlockId"] != serde_json::json!(wire.target_block_id)
                    || !attrs["filePath"].is_null()
                    || !attrs["region"].is_null()
                {
                    return Err("inline atom differs from its paired wire".into());
                }
            }
        }
        for child in node["content"].as_array().into_iter().flatten() {
            visit(child, expected, seen)?;
        }
        Ok(())
    }
    let mut seen = BTreeSet::new();
    visit(content, &expected, &mut seen)?;
    if seen.len() != wires.len() {
        return Err("paired inline atom is missing".into());
    }
    let mut sources = BTreeSet::new();
    for block in blocks.as_array().ok_or("missing projected blocks")? {
        for mark in block["marks"].as_array().into_iter().flatten() {
            if mark["type"] != "wikilink" {
                continue;
            }
            if let Some(wire) = mark["wireId"].as_str().and_then(|id| expected.get(id)) {
                if block["id"] != wire.source_block_id || !sources.insert(wire.id.clone()) {
                    return Err("inline source block differs from its paired wire".into());
                }
            }
        }
    }
    if sources.len() != wires.len() {
        return Err("paired source block is missing".into());
    }
    Ok(())
}
