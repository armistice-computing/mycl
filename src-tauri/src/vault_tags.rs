//! Imported tags are ordinary document hubs and exemplifies wires, not native tags.
use super::{identity, Binding, Plan};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

const MAX_TAG_REFERENCES: usize = 100_000;
const MAX_TAG_HUBS: usize = 10_000;
pub(crate) const PREDICATE: &str = "exemplifies";

#[derive(Debug, Default, Serialize)]
pub(crate) struct TagPlan {
    pub folder_id: Option<String>,
    pub folder_title: Option<String>,
    pub hubs: Vec<Hub>,
    pub references: Vec<Reference>,
    pub warnings: Vec<String>,
    pub omitted_warnings: usize,
}
impl TagPlan {
    fn warn(&mut self, message: String) {
        if self.warnings.len() < 128 {
            self.warnings.push(message);
        } else {
            self.omitted_warnings += 1;
        }
    }
}
#[derive(Debug, Serialize)]
pub(crate) struct Hub {
    pub id: String,
    pub title: String,
}
#[derive(Debug, Serialize)]
pub(crate) struct Reference {
    pub source: usize,
    pub target_id: String,
    pub name: String,
    /// None means front matter: document-level relation, no invented body block.
    pub range: Option<(usize, usize)>,
}

fn name(raw: &str) -> Option<&str> {
    let s = raw.trim().strip_prefix('#').unwrap_or(raw.trim());
    (!s.is_empty() && s.len() <= 4096 && !s.chars().any(|c| c.is_control() || c.is_whitespace()))
        .then_some(s)
}

/// Only plain Markdown text is eligible. Never mine code, URLs, HTML or links.
/// Require a literal source/text match so escapes and entities cannot create tags.
pub(crate) fn inline(body: &str) -> Vec<(usize, usize, String)> {
    inline_scan(body).0
}
fn inline_scan(body: &str) -> (Vec<(usize, usize, String)>, bool) {
    let mut result = vec![];
    let mut excluded = 0usize;
    let mut containers = vec![];
    let mut html_ranges = vec![];
    for (event, range) in Parser::new_ext(
        body,
        crate::crdt_engine::content_parse::md_options() | pulldown_cmark::Options::ENABLE_WIKILINKS,
    )
    .into_offset_iter()
    {
        match event {
            Event::Start(Tag::Paragraph | Tag::Heading { .. } | Tag::Item | Tag::TableCell) => {
                containers.push(range)
            }
            Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::TableCell,
            ) => {
                containers.pop();
            }
            Event::InlineHtml(_) => {
                // Mixed HTML/Markdown has no HTML tree in this parser. Keep the
                // enclosing text block literal instead of guessing tag nesting.
                html_ranges.push(containers.last().cloned().unwrap_or(0..body.len()));
            }
            Event::Start(
                Tag::CodeBlock(_) | Tag::Link { .. } | Tag::Image { .. } | Tag::HtmlBlock,
            ) => excluded += 1,
            Event::End(TagEnd::CodeBlock | TagEnd::Link | TagEnd::Image | TagEnd::HtmlBlock) => {
                excluded = excluded.saturating_sub(1)
            }
            Event::Text(text)
                if excluded == 0 && body.get(range.clone()) == Some(text.as_ref()) =>
            {
                for (offset, _) in text.match_indices('#') {
                    let start = range.start + offset;
                    if start > 0
                        && !body[..start]
                            .chars()
                            .next_back()
                            .is_some_and(char::is_whitespace)
                    {
                        continue;
                    }
                    let suffix = &body[start + 1..range.end];
                    let len = suffix
                        .char_indices()
                        .take_while(|(_, c)| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
                        .last()
                        .map_or(0, |(i, c)| i + c.len_utf8());
                    let tag = &suffix[..len];
                    if tag.chars().any(char::is_alphabetic) && name(tag).is_some() {
                        result.push((start, start + 1 + len, tag.to_owned()));
                    }
                }
            }
            _ => {}
        }
    }
    let saw_inline_html = !html_ranges.is_empty();
    html_ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<std::ops::Range<usize>> = vec![];
    for range in html_ranges {
        if let Some(last) = merged.last_mut().filter(|last| range.start <= last.end) {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    result.retain(|(start, _, _)| {
        let index = merged.partition_point(|range| range.end <= *start);
        !merged.get(index).is_some_and(|range| range.contains(start))
    });
    (result, saw_inline_html)
}

pub(crate) fn build(
    plan: &Plan,
    binding: &Binding,
    scan: &str,
    read: &mut impl FnMut(usize) -> Result<Vec<u8>, String>,
) -> Result<TagPlan, String> {
    let mut result = TagPlan::default();
    let mut candidates: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for doc in &plan.documents {
        for label in std::iter::once(doc.title.as_str())
            .chain(std::iter::once(super::stem(&doc.path)))
            .chain(doc.metadata.aliases.iter().map(String::as_str))
        {
            candidates
                .entry(crate::vault_preflight::path_key(label))
                .or_default()
                .insert(&doc.id);
        }
    }
    let mut targets = BTreeMap::<String, String>::new();
    let entries: BTreeMap<_, _> = plan.capture_entries.iter().map(|e| (e.index, e)).collect();
    for doc in &plan.documents {
        if doc.format != "markdown" {
            continue;
        }
        let bytes = read(doc.entry)?;
        let entry = entries.get(&doc.entry).ok_or("tag source missing")?;
        use sha2::Digest;
        if bytes.len() as u64 != entry.bytes
            || format!("{:x}", sha2::Sha256::digest(&bytes)) != entry.sha256
        {
            return Err("tag source integrity mismatch".into());
        }
        let source = std::str::from_utf8(&bytes).map_err(|_| "invalid tag source")?;
        let body = source
            .get(doc.metadata.body_start..doc.body_end)
            .ok_or("invalid tag body range")?;
        let mut seen = BTreeSet::new();
        for raw in &doc.metadata.tags {
            if name(raw).is_none() {
                result.warn(format!(
                    "{}: unsupported tag retained in original front matter: {raw}",
                    doc.path
                ));
            }
        }
        let mut refs: Vec<_> = doc
            .metadata
            .tags
            .iter()
            .filter_map(|raw| name(raw))
            .filter(|n| seen.insert(crate::vault_preflight::path_key(n)))
            .map(|n| (None, n.to_owned()))
            .collect();
        let (inline_tags, mixed_html) = inline_scan(body);
        if mixed_html {
            result.warn(format!("{}: inline tag conversion skipped in HTML-containing text blocks; original retained", doc.path));
        }
        refs.extend(inline_tags.into_iter().map(|(a, b, n)| {
            (
                Some((doc.metadata.body_start + a, doc.metadata.body_start + b)),
                n,
            )
        }));
        for (range, name) in refs {
            if result.references.len() >= MAX_TAG_REFERENCES {
                return Err("too many imported tag references".into());
            }
            let key = crate::vault_preflight::path_key(&name);
            let target_id = if let Some(id) = targets.get(&key) {
                id.clone()
            } else {
                let existing = candidates
                    .get(&key)
                    .filter(|ids| ids.len() == 1)
                    .and_then(|ids| ids.first())
                    .copied();
                let id = match existing {
                    Some(id) => id.to_owned(),
                    None => {
                        if result.hubs.len() >= MAX_TAG_HUBS
                            || result.hubs.len() + plan.documents.len() + plan.attachments.len()
                                >= 10_000
                        {
                            return Err("too many imported tag hubs".into());
                        }
                        let id = identity(binding, scan, "tag", &key);
                        if candidates.get(&key).is_some_and(|ids| ids.len() > 1) {
                            result.warn(format!(
                                "Tag {name}: ambiguous document name; created a dedicated hub"
                            ));
                        }
                        result.hubs.push(Hub {
                            id: id.clone(),
                            title: name.clone(),
                        });
                        id
                    }
                };
                targets.insert(key, id.clone());
                id
            };
            if target_id != doc.id {
                result.references.push(Reference {
                    source: doc.entry,
                    target_id,
                    name,
                    range,
                });
            }
        }
    }
    if !result.hubs.is_empty() {
        result.folder_id = Some(identity(binding, scan, "tag-folder", "Tags"));
        let mut title = "Tags".to_string();
        let mut suffix = 2;
        let occupied: BTreeSet<_> = plan
            .folders
            .iter()
            .filter(|f| f.parent_id == plan.root_id)
            .map(|f| crate::vault_preflight::path_key(&f.path))
            .collect();
        while occupied.contains(&crate::vault_preflight::path_key(&title)) {
            title = format!("Tags ({suffix})");
            suffix += 1;
        }
        result.folder_title = Some(title);
    }
    Ok(result)
}
