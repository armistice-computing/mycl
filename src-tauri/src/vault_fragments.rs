//! Source anchors name actual projected blocks; historical IDs are never reused.
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Anchors {
    pub headings: BTreeMap<String, BTreeSet<String>>,
    pub slugs: BTreeMap<String, BTreeSet<String>>,
    pub blocks: BTreeMap<String, BTreeSet<String>>,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Heading {
    pub text: String,
    pub level: u64,
    pub id: String,
}
fn key(s: &str) -> String {
    crate::vault_preflight::path_key(&s.split_whitespace().collect::<Vec<_>>().join(" "))
}
fn slug(s: &str) -> String {
    key(s)
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}
fn add(
    index: &mut BTreeMap<String, BTreeSet<String>>,
    name: String,
    id: &str,
    budget: &mut usize,
) -> Result<(), String> {
    if index.get(&name).is_some_and(|ids| ids.contains(id)) {
        return Ok(());
    }
    *budget = budget
        .checked_sub(name.len() + id.len() + 64)
        .ok_or("anchor index exceeds 8 MiB bound")?;
    index.entry(name).or_default().insert(id.to_owned());
    Ok(())
}
impl Anchors {
    pub(crate) fn resolve(&self, raw: &str, wiki: bool) -> Option<String> {
        // Split literal hierarchy separators before decoding: %23 is name data.
        let raw_parts: Vec<String> = if wiki {
            raw.split('#').map(str::to_owned).collect()
        } else {
            vec![raw.to_owned()]
        };
        let mut forms = vec![];
        if wiki {
            forms.push(raw_parts.clone());
        }
        if let Some(decoded) = raw_parts
            .iter()
            .map(|s| crate::vault_plan::decode_url_path(s))
            .collect::<Option<Vec<_>>>()
        {
            forms.push(decoded);
        }
        let mut candidates = BTreeSet::new();
        for form in forms {
            if form[0].starts_with('^') {
                if form.len() == 1 {
                    if let Some(ids) = self.blocks.get(&form[0][1..]) {
                        candidates.extend(ids.iter().cloned());
                    }
                }
                continue;
            }
            let parts: Vec<_> = form.iter().map(|s| key(s)).collect();
            if parts.iter().any(String::is_empty) {
                continue;
            }
            let normalized = serde_json::to_string(&parts).ok()?;
            if let Some(ids) = self.headings.get(&normalized) {
                candidates.extend(ids.iter().cloned());
            }
            if !wiki && parts.len() == 1 {
                if let Some(ids) = self.slugs.get(&parts[0]) {
                    candidates.extend(ids.iter().cloned());
                }
            }
        }
        (candidates.len() == 1).then(|| candidates.first().unwrap().clone())
    }
}
fn shape(blocks: &Value) -> Vec<Value> {
    blocks
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| {
            serde_json::json!([
                b["id"],
                b["type"],
                b["parentId"],
                b["level"],
                b["checked"],
                b["language"]
            ])
        })
        .collect()
}

pub(crate) fn discover(
    body: &str,
    content: &Value,
    document: &str,
    operation: &str,
) -> Result<Anchors, String> {
    let base = crate::crdt_engine::projection::materialize_tiptap_json(content, document);
    let mut anchors = Anchors::default();
    let mut budget = 8 * 1024 * 1024;
    let mut path: Vec<Heading> = vec![];
    for block in base
        .blocks_json
        .as_array()
        .ok_or("missing anchor projection")?
    {
        if block["type"] == "heading" {
            let heading = Heading {
                text: block["content"].as_str().unwrap_or("").into(),
                level: block["level"].as_u64().unwrap_or(1),
                id: block["id"].as_str().ok_or("missing heading id")?.into(),
            };
            while path.last().is_some_and(|h| h.level >= heading.level) {
                path.pop();
            }
            path.push(heading);
            let id = &path.last().unwrap().id;
            for from in 0..path.len() {
                let parts = path[from..]
                    .iter()
                    .map(|h| key(&h.text))
                    .collect::<Vec<_>>();
                let name = serde_json::to_string(&parts).map_err(|e| e.to_string())?;
                if !name.is_empty() && name.len() <= 4096 {
                    add(&mut anchors.headings, name, id, &mut budget)?;
                }
            }
            let name = slug(&path.last().unwrap().text);
            if !name.is_empty() && name.len() <= 4096 {
                add(&mut anchors.slugs, name, id, &mut budget)?;
            }
        }
    }
    let mut containers = vec![];
    let mut excluded = 0usize;
    let mut markers = vec![];
    let mut html_ranges = vec![];
    for (event, range) in Parser::new_ext(
        body,
        crate::crdt_engine::content_parse::md_options() | pulldown_cmark::Options::ENABLE_WIKILINKS,
    )
    .into_offset_iter()
    {
        match event {
            Event::Start(Tag::Paragraph | Tag::Item) => containers.push(range),
            Event::End(TagEnd::Paragraph | TagEnd::Item) => {
                containers.pop();
            }
            Event::Start(
                Tag::CodeBlock(_)
                | Tag::Link { .. }
                | Tag::Image { .. }
                | Tag::HtmlBlock
                | Tag::Heading { .. },
            ) => excluded += 1,
            Event::End(
                TagEnd::CodeBlock
                | TagEnd::Link
                | TagEnd::Image
                | TagEnd::HtmlBlock
                | TagEnd::Heading(_),
            ) => excluded = excluded.saturating_sub(1),
            Event::InlineHtml(_) => {
                html_ranges.push(containers.last().cloned().unwrap_or(0..body.len()))
            }
            Event::Text(text)
                if excluded == 0 && body.get(range.clone()) == Some(text.as_ref()) =>
            {
                let Some(owner) = containers.last() else {
                    continue;
                };
                let trimmed = text.trim_end();
                let Some(at) = trimmed.rfind('^') else {
                    continue;
                };
                let id = &trimmed[at + 1..];
                let start = range.start + at;
                let end = start + 1 + id.len();
                if id.is_empty()
                    || id.len() > 4096
                    || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    continue;
                }
                if start == 0
                    || !body[..start]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace)
                {
                    continue;
                }
                if body[owner.start..start].trim().is_empty()
                    || !body[end..owner.end].trim().is_empty()
                {
                    continue;
                }
                markers.push((start, end, id.to_owned()));
            }
            _ => {}
        }
    }
    html_ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<std::ops::Range<usize>> = vec![];
    for range in html_ranges {
        if let Some(last) = merged.last_mut().filter(|last| range.start <= last.end) {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    markers.retain(|(start, _, _)| {
        let i = merged.partition_point(|r| r.end <= *start);
        !merged.get(i).is_some_and(|r| r.contains(start))
    });
    if markers.len() > 100_000 {
        return Err("too many block anchors".into());
    }
    if markers.is_empty() {
        return Ok(anchors);
    }
    let prefix = format!("MYCL_ANCHOR_{:x}X", Sha256::digest(body.as_bytes()));
    if body.contains(&prefix)
        || serde_json::to_string(content)
            .map_err(|e| e.to_string())?
            .contains(&prefix)
    {
        return Err("anchor probe namespace collision".into());
    }
    let mut rewritten = String::new();
    let mut at = 0;
    for (i, (start, end, _)) in markers.iter().enumerate() {
        rewritten.push_str(&body[at..*start]);
        rewritten.push_str(&format!("{prefix}{i}Z"));
        at = *end;
    }
    rewritten.push_str(&body[at..]);
    let probe = crate::crdt_engine::content_parse::parse_write_content_for_operation(
        &rewritten,
        Some("markdown"),
        operation,
    )?;
    let projection =
        crate::crdt_engine::projection::materialize_tiptap_json(&probe.tiptap_json, document);
    if shape(&projection.blocks_json) != shape(&base.blocks_json) {
        return Err("anchor probe changed block structure".into());
    }
    let mut owners: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    for block in projection.blocks_json.as_array().unwrap() {
        let text = block["content"].as_str().unwrap_or("");
        for (offset, _) in text.match_indices(&prefix) {
            let tail = &text[offset + prefix.len()..];
            let Some((number, _)) = tail.split_once('Z') else {
                continue;
            };
            if let Ok(index) = number.parse::<usize>() {
                if index < markers.len() {
                    owners
                        .entry(index)
                        .or_default()
                        .insert(block["id"].as_str().ok_or("missing anchor owner")?.into());
                }
            }
        }
    }
    for (i, (_, _, id)) in markers.into_iter().enumerate() {
        for owner in owners.remove(&i).unwrap_or_default() {
            add(&mut anchors.blocks, id.clone(), &owner, &mut budget)?;
        }
    }
    Ok(anchors)
}
