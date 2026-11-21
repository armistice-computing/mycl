//! Pure Markdown/Obsidian-style planning. No graph/file mutation or source-path
//! opening; callers supply captured entry metadata and a bounded byte reader.
use crate::{
    vault_capture::Binding,
    vault_preflight::{Admission, Kind, Limits},
};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const MAX_METADATA: usize = 32 * 1024 * 1024;
const MAX_FRONT_MATTER: usize = 64 * 1024;
const MAX_LINKS: usize = 100_000;
const MAX_MARKERS: usize = 100_000;
const MAX_FIELD: usize = 4096;

#[path = "vault_plan_export.rs"]
mod export;
#[path = "vault_tags.rs"]
pub(crate) mod tags;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Input {
    pub index: usize,
    pub path: String,
    pub kind: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Issue {
    pub path: String,
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Metadata {
    pub title: Option<String>,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Exact header including delimiters; unmapped fields are never discarded.
    pub front_matter: Option<String>,
    /// Byte offset into the original UTF-8 source, not a character offset.
    pub body_start: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct Document {
    pub entry: usize,
    pub id: String,
    pub path: String,
    pub parent_id: String,
    pub title: String,
    pub format: String,
    pub metadata: Metadata,
    /// Exclusive byte boundary; an exactly recognized export footer can be omitted.
    pub body_end: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct Folder {
    pub id: String,
    pub path: String,
    pub parent_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Link {
    pub source: usize,
    pub target: Option<usize>,
    pub target_kind: Option<String>,
    pub raw: String,
    pub fragment: Option<String>,
    pub embedded: bool,
    pub wiki: bool,
    pub source_offset: usize,
}
impl Link {
    /// Only a resolved whole-target reference is currently a faithful wire.
    /// Fragments/embeds remain in the source; never silently widen them to a
    /// document-level semantic assertion.
    pub(crate) fn whole_target(&self) -> Option<usize> {
        if self.fragment.is_some() || self.embedded {
            return None;
        }
        self.target
            .filter(|_| matches!(self.target_kind.as_deref(), Some("document" | "artifact")))
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Plan {
    /// Captured identity, not just positional references into a future reader.
    pub capture_entries: Vec<Input>,
    pub root_id: String,
    pub folder_name: String,
    pub folders: Vec<Folder>,
    pub documents: Vec<Document>,
    pub attachments: Vec<usize>,
    pub links: Vec<Link>,
    pub issues: Vec<Issue>,
    pub export_wires: Vec<export::ExportWire>,
    pub tags: tags::TagPlan,
}

fn issue(issues: &mut Vec<Issue>, path: &str, kind: &str, message: impl Into<String>) {
    issues.push(Issue {
        path: path.into(),
        kind: kind.into(),
        message: message.into(),
    });
}

fn identity(binding: &Binding, scan: &str, kind: &str, path: &str) -> String {
    let bytes =
        serde_json::to_vec(&(binding, scan, kind, path)).expect("serializable identity tuple");
    format!("vault-{kind}-{:x}", Sha256::digest(bytes))
}

fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name)
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

fn scalar(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_FIELD {
        return None;
    }
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value).ok();
    }
    if value.starts_with('\'') {
        return value
            .strip_prefix('\'')?
            .strip_suffix('\'')
            .map(|s| s.replace("''", "'"));
    }
    if value.starts_with(['[', '{', '|', '>', '&', '*', '!', '#'])
        || value.ends_with(':')
        || value.contains(": ")
    {
        return None;
    }
    Some(value.split(" #").next()?.trim_end().into())
}

fn inline_list(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    if !value.starts_with('[') {
        return Some(vec![scalar(value)?]);
    }
    let body = value.strip_prefix('[')?.strip_suffix(']')?;
    if body.trim().is_empty() {
        return Some(vec![]);
    }
    let mut parts = vec![];
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (offset, ch) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('"') && ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == ',' {
            parts.push(scalar(&body[start..offset])?);
            start = offset + 1;
        } else if ch == '[' || ch == '{' {
            return None;
        }
        if parts.len() > 256 {
            return None;
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    parts.push(scalar(&body[start..])?);
    Some(parts)
}

fn metadata(source: &str, path: &str, issues: &mut Vec<Issue>) -> Result<Metadata, String> {
    let mut metadata = Metadata::default();
    let mut lines = source.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if first.trim_start_matches('\u{feff}').trim_end() != "---" {
        return Ok(metadata);
    }
    let mut end = first.len();
    if end > MAX_FRONT_MATTER {
        return Err(format!("{path}: front matter exceeds planning bound"));
    }
    let mut header = None;
    for line in lines {
        if end + line.len() > MAX_FRONT_MATTER {
            return Err(format!("{path}: front matter exceeds planning bound"));
        }
        if line.trim_end() == "---" {
            header = Some(&source[first.len()..end]);
            end += line.len();
            break;
        }
        end += line.len();
        if end > MAX_FRONT_MATTER {
            return Err(format!("{path}: front matter exceeds planning bound"));
        }
    }
    let Some(header) = header else {
        return Ok(metadata);
    };
    let rows: Vec<_> = header.lines().collect();
    let mut fields = BTreeMap::<String, Option<Vec<String>>>::new();
    for (index, line) in rows.iter().enumerate() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.is_empty()
            || key.len() > 128
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || (!value.is_empty() && !value.starts_with(char::is_whitespace))
        {
            continue;
        }
        let key = key.to_ascii_lowercase();
        if fields.len() >= 128 {
            return Err(format!("{path}: too many front matter keys"));
        }
        if fields.contains_key(&key) {
            fields.insert(key.clone(), None);
            issue(
                issues,
                path,
                "duplicate_metadata",
                format!("Duplicate front matter key {key}; not promoted"),
            );
            continue;
        }
        let parsed = if matches!(key.as_str(), "aliases" | "tags") {
            if value.trim().is_empty() {
                let mut values = Vec::new();
                let mut valid = true;
                for row in rows.iter().skip(index + 1) {
                    if row.trim().is_empty() || row.trim_start().starts_with('#') {
                        continue;
                    }
                    if let Some(item) = row.trim_start().strip_prefix("- ") {
                        match scalar(item) {
                            Some(value) => values.push(value),
                            None => valid = false,
                        }
                    } else if !row.starts_with(char::is_whitespace) {
                        break;
                    } else {
                        valid = false;
                    }
                    if values.len() > 256 {
                        valid = false;
                        break;
                    }
                }
                valid.then_some(values)
            } else {
                inline_list(value)
            }
        } else {
            // Do not promote only the first line of a folded/nested YAML value.
            let continuation = rows
                .iter()
                .skip(index + 1)
                .find(|row| !row.trim().is_empty() && !row.trim_start().starts_with('#'));
            if continuation.is_some_and(|row| row.starts_with(char::is_whitespace)) {
                None
            } else {
                scalar(value).map(|v| vec![v])
            }
        };
        fields.insert(key, parsed);
    }
    if fields.is_empty() {
        return Ok(metadata);
    } // ambiguous horizontal rules stay body text
    metadata.front_matter = Some(source[..end].into());
    metadata.body_start = end;
    metadata.title = fields
        .get("title")
        .and_then(|v| v.as_ref())
        .and_then(|v| v.first())
        .filter(|s| !s.is_empty())
        .cloned();
    metadata.aliases = fields
        .get("aliases")
        .and_then(|v| v.clone())
        .unwrap_or_default();
    metadata.tags = fields
        .get("tags")
        .and_then(|v| v.clone())
        .unwrap_or_default();
    for (names, destination) in [
        (
            &["created", "created_at", "createdat"][..],
            &mut metadata.created_at,
        ),
        (
            &["modified", "updated", "updated_at", "updatedat"][..],
            &mut metadata.updated_at,
        ),
    ] {
        let values: BTreeSet<_> = names
            .iter()
            .filter_map(|key| {
                fields
                    .get(*key)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.first())
            })
            .collect();
        if values.len() == 1 {
            let raw = *values.first().unwrap();
            if chrono::DateTime::parse_from_rfc3339(raw)
                .is_ok_and(|date| date.timestamp_millis() >= 0)
            {
                *destination = Some(raw.clone());
            } else {
                issue(
                    issues,
                    path,
                    "ambiguous_date",
                    format!("{} retained without inventing a time or timezone", names[0]),
                );
            }
        } else if values.len() > 1 {
            issue(
                issues,
                path,
                "conflicting_dates",
                format!("Conflicting {} fields retained", names[0]),
            );
        }
    }
    if let (Some(created), Some(updated)) = (&metadata.created_at, &metadata.updated_at) {
        if chrono::DateTime::parse_from_rfc3339(updated).unwrap()
            < chrono::DateTime::parse_from_rfc3339(created).unwrap()
        {
            metadata.created_at = None;
            metadata.updated_at = None;
            issue(
                issues,
                path,
                "conflicting_dates",
                "Updated precedes created; both dates retained without promotion",
            );
        }
    }
    for (key, value) in fields {
        if value.is_none()
            || !matches!(
                key.as_str(),
                "title"
                    | "aliases"
                    | "tags"
                    | "created"
                    | "created_at"
                    | "createdat"
                    | "modified"
                    | "updated"
                    | "updated_at"
                    | "updatedat"
            )
        {
            issue(
                issues,
                path,
                "retained_metadata",
                format!("Front matter field {key} retained verbatim"),
            );
        }
    }
    Ok(metadata)
}

pub(crate) fn decode_url_path(raw: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut iter = raw.as_bytes().iter().copied();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let a = (iter.next()? as char).to_digit(16)?;
            let b = (iter.next()? as char).to_digit(16)?;
            bytes.push((a * 16 + b) as u8);
        } else {
            bytes.push(byte);
        }
    }
    let value = String::from_utf8(bytes).ok()?;
    (!value.chars().any(char::is_control)).then_some(value)
}

fn relative_path(base: &str, target: &str) -> Option<String> {
    if target.starts_with('/') || target.contains('\\') {
        return None;
    }
    let mut parts: Vec<_> = base.split('/').filter(|s| !s.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

pub(crate) fn build(
    entries: &[Input],
    binding: &Binding,
    scan: &str,
    folder_name: &str,
    mut read_note: impl FnMut(usize) -> Result<Vec<u8>, String>,
) -> Result<Plan, String> {
    if folder_name.trim().is_empty()
        || folder_name.len() > MAX_FIELD
        || folder_name.chars().any(char::is_control)
    {
        return Err("invalid import folder name".into());
    }
    let root_id = identity(binding, scan, "folder", "");
    let mut plan = Plan {
        capture_entries: vec![],
        root_id: root_id.clone(),
        folder_name: folder_name.trim().into(),
        folders: vec![],
        documents: vec![],
        attachments: vec![],
        links: vec![],
        issues: vec![],
        export_wires: vec![],
        tags: tags::TagPlan::default(),
    };
    let mut admission = Admission::new(Limits::default());
    let mut indices = BTreeSet::new();
    let mut folders = BTreeSet::new();
    let mut paths = BTreeMap::new();
    let mut names = BTreeMap::<String, BTreeSet<usize>>::new();
    let mut entry_paths = BTreeMap::new();
    let mut used = 0usize;
    // Capture indices address the complete receipt vector, not arbitrary keys.
    if entries.iter().any(|entry| entry.index >= entries.len()) {
        return Err("captured entry index out of range".into());
    }
    let exported = export::recognized(entries, &mut read_note)?;
    for entry in entries {
        if !indices.insert(entry.index) {
            return Err("duplicate captured entry index".into());
        }
        let (path, kind) = admission.admit(&entry.path, entry.kind == "directory")?;
        let expected = match kind {
            Kind::Directory => "directory",
            Kind::Note => "note",
            Kind::Attachment => "attachment",
        };
        if expected != entry.kind {
            return Err("captured entry kind mismatch".into());
        }
        if kind == Kind::Directory {
            if entry.bytes != 0 || !entry.sha256.is_empty() {
                return Err("invalid captured directory metadata".into());
            }
        } else if entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid captured content hash".into());
        }
        let mut bytes = 0;
        admission.account(
            kind,
            &mut bytes,
            usize::try_from(entry.bytes).map_err(|_| "captured size overflow")?,
        )?;
        plan.capture_entries.push(entry.clone());
        used = used.checked_add(path.len()).ok_or("plan size overflow")?;
        let mut directory = if kind == Kind::Directory {
            path.as_str()
        } else {
            parent(&path)
        };
        while !directory.is_empty() {
            folders.insert(directory.to_owned());
            directory = parent(directory);
            if folders.len() + entries.len() > 20_000 {
                return Err("too many planned folders/entities".into());
            }
        }
        if kind == Kind::Directory {
            continue;
        }
        paths.insert(crate::vault_preflight::path_key(&path), entry.index);
        entry_paths.insert(entry.index, path.clone());
        let parent_id = if parent(&path).is_empty() {
            root_id.clone()
        } else {
            identity(binding, scan, "folder", parent(&path))
        };
        let mut keys = vec![path.rsplit('/').next().unwrap().to_owned()];
        if kind == Kind::Note && !(exported && path.starts_with("attachments/")) {
            if entry.bytes > 32 * 1024 * 1024 {
                return Err("note exceeds planning limit".into());
            }
            let bytes = read_note(entry.index)?;
            if bytes.len() as u64 != entry.bytes
                || format!("{:x}", Sha256::digest(&bytes)) != entry.sha256
            {
                return Err("captured note integrity mismatch".into());
            }
            let source = match std::str::from_utf8(&bytes) {
                Ok(source) => source,
                Err(_) => {
                    plan.attachments.push(entry.index);
                    issue(
                        &mut plan.issues,
                        &path,
                        "invalid_utf8",
                        "Retain this source as an attachment, not lossy text",
                    );
                    for key in [keys[0].as_str(), stem(&path)] {
                        names
                            .entry(crate::vault_preflight::path_key(key))
                            .or_default()
                            .insert(entry.index);
                    }
                    continue;
                }
            };
            let plain = path.to_ascii_lowercase().ends_with(".txt");
            let metadata = if plain {
                Metadata::default()
            } else {
                metadata(source, &path, &mut plan.issues)?
            };
            used += metadata.front_matter.as_ref().map_or(0, String::len);
            let title = metadata.title.clone().unwrap_or_else(|| stem(&path).into());
            keys.push(stem(&path).into());
            keys.push(title.clone());
            keys.extend(metadata.aliases.clone());
            for key in &keys {
                used += key.len();
            }
            if used > MAX_METADATA {
                return Err("plan metadata exceeds limit".into());
            }
            if !plain {
                let body = &source[metadata.body_start..];
                if body
                    .bytes()
                    .filter(|b| b"*_[]<>~`|#\n\r".contains(b))
                    .take(MAX_MARKERS + 1)
                    .count()
                    > MAX_MARKERS
                {
                    return Err(format!("{path}: Markdown is too structurally complex"));
                }
                for (event, range) in Parser::new_ext(
                    body,
                    crate::crdt_engine::content_parse::md_options() | Options::ENABLE_WIKILINKS,
                )
                .into_offset_iter()
                {
                    let (link_type, raw, embedded) = match event {
                        Event::Start(Tag::Link {
                            link_type,
                            dest_url,
                            ..
                        }) => (link_type, dest_url, false),
                        Event::Start(Tag::Image {
                            link_type,
                            dest_url,
                            ..
                        }) => (link_type, dest_url, true),
                        _ => continue,
                    };
                    if raw.len() > MAX_FIELD || plan.links.len() >= MAX_LINKS {
                        return Err("link planning limit exceeded".into());
                    }
                    if !matches!(link_type, LinkType::WikiLink { .. })
                        && url::Url::parse(&raw).is_ok()
                    {
                        continue;
                    }
                    used += raw.len();
                    if used > MAX_METADATA {
                        return Err("plan metadata exceeds limit".into());
                    }
                    let (target, fragment) = raw
                        .split_once('#')
                        .map(|(p, f)| (p, Some(f.to_owned())))
                        .unwrap_or((&raw, None));
                    plan.links.push(Link {
                        source: entry.index,
                        target: None,
                        target_kind: None,
                        raw: target.into(),
                        fragment,
                        embedded,
                        wiki: matches!(link_type, LinkType::WikiLink { .. }),
                        source_offset: metadata.body_start + range.start,
                    });
                }
            }
            plan.documents.push(Document {
                entry: entry.index,
                id: identity(binding, scan, "doc", &path),
                path: path.clone(),
                parent_id,
                title,
                format: if plain { "plain" } else { "markdown" }.into(),
                metadata,
                body_end: source.len(),
            });
        } else {
            plan.attachments.push(entry.index);
        }
        for key in keys {
            names
                .entry(crate::vault_preflight::path_key(&key))
                .or_default()
                .insert(entry.index);
        }
    }
    for path in folders {
        let parent_id = if parent(&path).is_empty() {
            root_id.clone()
        } else {
            identity(binding, scan, "folder", parent(&path))
        };
        plan.folders.push(Folder {
            id: identity(binding, scan, "folder", &path),
            path,
            parent_id,
        });
    }
    if exported {
        export::augment(&mut plan, entries, &mut read_note)?;
    }
    for link in &mut plan.links {
        let source_path = &entry_paths[&link.source];
        let Some(target) = decode_url_path(&link.raw) else {
            issue(
                &mut plan.issues,
                source_path,
                "invalid_link",
                "Link encoding retained but not resolved",
            );
            continue;
        };
        if target.is_empty() {
            link.target = Some(link.source);
            continue;
        }
        let base = if link.wiki && !target.starts_with('.') {
            ""
        } else {
            parent(source_path)
        };
        let Some(relative) = relative_path(base, &target) else {
            issue(
                &mut plan.issues,
                source_path,
                "outside_source",
                "Link leaves the captured source; no filesystem lookup performed",
            );
            continue;
        };
        let mut candidates = BTreeSet::new();
        if link.wiki && !target.contains('/') && !target.starts_with('.') {
            if let Some(matches) = names.get(&crate::vault_preflight::path_key(&target)) {
                candidates.extend(matches);
            }
        } else {
            if let Some(index) = paths.get(&crate::vault_preflight::path_key(&relative)) {
                candidates.insert(*index);
            }
            if link.wiki && candidates.is_empty() {
                for suffix in [".md", ".markdown"] {
                    if let Some(index) = paths.get(&crate::vault_preflight::path_key(&format!(
                        "{relative}{suffix}"
                    ))) {
                        candidates.insert(*index);
                    }
                }
            }
        }
        if candidates.len() > 1 {
            issue(
                &mut plan.issues,
                source_path,
                "ambiguous_link",
                format!("Multiple captured targets for {}", link.raw),
            );
            continue;
        }
        link.target = candidates.first().copied();
        if link.target.is_none() {
            issue(
                &mut plan.issues,
                source_path,
                "unresolved_link",
                format!("No captured target for {}", link.raw),
            );
        }
    }
    let documents: BTreeSet<_> = plan.documents.iter().map(|d| d.entry).collect();
    for link in &mut plan.links {
        link.target_kind = link.target.map(|index| {
            if documents.contains(&index) {
                "document"
            } else {
                "artifact"
            }
            .into()
        });
    }
    for link in &plan.links {
        if link.fragment.is_some() || link.embedded {
            issue(
                &mut plan.issues,
                &entry_paths[&link.source],
                if link.embedded { "retained_reference" } else { "fragment_reference" },
                format!(
                    "Reference at byte {} requires endpoint/embed mapping; if not mapped it stays as written: {}{}{}",
                    link.source_offset,
                    link.raw,
                    if link.fragment.is_some() { "#" } else { "" },
                    link.fragment.as_deref().unwrap_or("")
                ),
            );
        }
    }
    plan.tags = tags::build(&plan, binding, scan, &mut read_note)?;
    // Count serialization without allocating a second copy of a large plan.
    struct Bound(usize);
    impl std::io::Write for Bound {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("serialized plan exceeds limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Bound(MAX_METADATA), &plan).map_err(|e| e.to_string())?;
    Ok(plan)
}

#[cfg(test)]
#[path = "vault_plan_tests.rs"]
mod tests;
