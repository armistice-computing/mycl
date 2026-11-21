//! Immutable note preparation for an unpublished graph. No filesystem or graph
//! writes; the caller owns capture authorization and durable custody.
use crate::vault_plan::{Document, Input};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[path = "vault_fragments.rs"]
pub(crate) mod fragments;
#[path = "vault_images.rs"]
pub(crate) mod images;
pub(crate) const PARSER_VERSION: u32 = 5;

#[derive(Debug, Serialize)]
pub(crate) struct PreparedNote {
    pub parser_version: u32,
    pub document_id: String,
    pub title: String,
    pub source_path: String,
    pub source_sha256: String,
    pub metadata: crate::vault_plan::Metadata,
    pub body_start: usize,
    pub body_end: usize,
    pub tiptap_json: Value,
    pub warnings: Vec<String>,
    pub inline_wires: Vec<crate::vault_inline_links::InlineWire>,
    pub anchors: fragments::Anchors,
    pub images: Vec<images::Image>,
    /// Hash of canonical semantic preparation, not a Y.Doc encoding.
    pub preparation_sha256: String,
    /// Full original, including all metadata and any omitted export footer.
    /// Persist as original bytes, not JSON's array-of-integers representation.
    #[serde(skip)]
    pub original: Vec<u8>,
}

pub(crate) fn prepare(
    document: &Document,
    entry: &Input,
    original: Vec<u8>,
) -> Result<PreparedNote, String> {
    prepare_with_images(document, entry, original, vec![], vec![])
}
pub(crate) fn prepare_with_images(
    document: &Document,
    entry: &Input,
    original: Vec<u8>,
    images: Vec<images::Image>,
    image_warnings: Vec<String>,
) -> Result<PreparedNote, String> {
    if entry.kind != "note" || document.entry != entry.index || document.path != entry.path {
        return Err("prepared note does not match captured entry".into());
    }
    if original.len() > 32 * 1024 * 1024
        || original.len() as u64 != entry.bytes
        || format!("{:x}", Sha256::digest(&original)) != entry.sha256
    {
        return Err("prepared note source integrity mismatch".into());
    }
    if document.format != "markdown" {
        return Err("unsupported vault note format".into());
    }
    crate::ids::validate_local_id(&document.id, "document_id")?;
    let source = std::str::from_utf8(&original).map_err(|_| "prepared note is not UTF-8")?;
    let body = source
        .get(document.metadata.body_start..document.body_end)
        .ok_or("invalid planned note body range")?;
    let rendered = images::rewrite(body, &images, std::iter::empty())?;
    let parsed = crate::crdt_engine::content_parse::parse_write_content_for_operation(
        &rendered,
        Some("markdown"),
        &format!("vault-note-v{PARSER_VERSION}:{}", document.id),
    )?;
    let anchors = fragments::discover(
        &rendered,
        &parsed.tiptap_json,
        &document.id,
        &format!("vault-note-v{PARSER_VERSION}:{}", document.id),
    )?;
    let mut warnings = parsed.warnings;
    warnings.extend(image_warnings);
    if document.metadata.created_at.is_none() {
        if let Some(modified) = &document.metadata.updated_at {
            warnings.push(format!("Without a usable source creation date, system timestamps use import time; the source modified date {modified} is retained in the original"));
        }
    }
    if !images.is_empty() {
        let missing = images::validate(&parsed.tiptap_json, &images)?;
        if missing > 0 {
            warnings.push(format!("{missing} image occurrences rendered as alt text in inline-only contexts; originals retained"));
        }
    }
    if !anchors.blocks.is_empty() {
        warnings.push("Obsidian block-ID marker text is retained; uniquely mapped IDs support native navigation".into());
    }
    let mut prepared = PreparedNote {
        images,
        anchors,
        parser_version: PARSER_VERSION,
        document_id: document.id.clone(),
        title: document.title.clone(),
        source_path: entry.path.clone(),
        source_sha256: entry.sha256.clone(),
        metadata: document.metadata.clone(),
        body_start: document.metadata.body_start,
        body_end: document.body_end,
        tiptap_json: parsed.tiptap_json,
        warnings,
        inline_wires: vec![],
        preparation_sha256: String::new(),
        original,
    };
    prepared.preparation_sha256 = prepared.digest()?;
    Ok(prepared)
}

impl PreparedNote {
    pub(crate) fn install_inline_links(
        &mut self,
        content: Value,
        wires: Vec<crate::vault_inline_links::InlineWire>,
        warnings: Vec<String>,
    ) -> Result<(), String> {
        self.verify()?;
        self.tiptap_json = content;
        self.inline_wires = wires;
        self.warnings.extend(warnings);
        self.preparation_sha256 = self.digest()?;
        Ok(())
    }
    pub(crate) fn verify(&self) -> Result<(), String> {
        if self.parser_version != PARSER_VERSION
            || self.digest()? != self.preparation_sha256
            || self.original.len() > 32 * 1024 * 1024
            || format!("{:x}", Sha256::digest(&self.original)) != self.source_sha256
            || self.body_start != self.metadata.body_start
        {
            return Err("prepared note integrity mismatch".into());
        }
        std::str::from_utf8(&self.original)
            .ok()
            .and_then(|s| s.get(self.body_start..self.body_end))
            .ok_or("invalid prepared body range")?;
        Ok(())
    }

    fn digest(&self) -> Result<String, String> {
        // Sort object keys recursively, independent of preserve_order.
        fn canonical(value: &Value) -> Value {
            match value {
                Value::Object(map) => {
                    let sorted: std::collections::BTreeMap<_, _> = map.iter().collect();
                    Value::Object(
                        sorted
                            .into_iter()
                            .map(|(k, v)| (k.clone(), canonical(v)))
                            .collect(),
                    )
                }
                Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
                other => other.clone(),
            }
        }
        let semantic = canonical(&serde_json::json!({
            "parserVersion": self.parser_version, "documentId": self.document_id,
            "title": self.title, "sourcePath": self.source_path, "sourceSha256": self.source_sha256,
            "bodyStart": self.body_start, "bodyEnd": self.body_end,
            "content": self.tiptap_json, "metadata": self.metadata,
            "warnings": self.warnings,
            "inlineWires": self.inline_wires,
            "anchors": self.anchors,
            "images": self.images,
        }));
        let digest = Sha256::digest(serde_json::to_vec(&semantic).map_err(|e| e.to_string())?);
        Ok(format!("{digest:x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(text: &str) -> (crate::vault_plan::Plan, Input) {
        let entry = Input {
            index: 0,
            path: "note.md".into(),
            kind: "note".into(),
            bytes: text.len() as u64,
            sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
        };
        let binding = crate::vault_capture::Binding {
            graph: "capture-context".into(),
            incarnation: "one".into(),
            principal: "human:owner".into(),
        };
        let plan = crate::vault_plan::build(&[entry.clone()], &binding, "scan", "Import", |_| {
            Ok(text.as_bytes().to_vec())
        })
        .unwrap();
        (plan, entry)
    }
    #[test]
    fn repeated_preparation_keeps_ids_digest_and_original() {
        let text = "---\ntitle: My note\ncustom: retain me\n---\n# Heading\n\nBody.";
        let (mut plan, entry) = source(text);
        let a = prepare(&plan.documents[0], &entry, text.as_bytes().to_vec()).unwrap();
        let b = prepare(&plan.documents[0], &entry, text.as_bytes().to_vec()).unwrap();
        assert_eq!(a.tiptap_json, b.tiptap_json);
        assert_eq!(a.preparation_sha256, b.preparation_sha256);
        assert_eq!(a.original, text.as_bytes());
        assert!(!a.tiptap_json.to_string().contains("retain me"));
        assert!(a.tiptap_json.to_string().contains("data-block-id"));
        assert!(serde_json::to_value(&a).unwrap().get("original").is_none());
        assert!(a
            .metadata
            .front_matter
            .as_ref()
            .unwrap()
            .contains("custom: retain me"));
        plan.documents[0].metadata.tags.push("mapped-tag".into());
        let changed = prepare(&plan.documents[0], &entry, text.as_bytes().to_vec()).unwrap();
        assert_eq!(a.tiptap_json, changed.tiptap_json);
        assert_ne!(a.preparation_sha256, changed.preparation_sha256);
    }
    #[test]
    fn source_changes_and_entry_swaps_are_refused() {
        let (plan, mut entry) = source("Body.");
        assert!(prepare(&plan.documents[0], &entry, b"Other".to_vec()).is_err());
        entry.index = 1;
        assert!(prepare(&plan.documents[0], &entry, b"Body.".to_vec()).is_err());
    }
    #[test]
    fn invalid_utf8_boundary_is_refused_without_panicking() {
        let (mut plan, entry) = source("café");
        plan.documents[0].body_end = 4;
        assert!(
            prepare(&plan.documents[0], &entry, "café".as_bytes().to_vec())
                .unwrap_err()
                .contains("range")
        );
    }
    #[test]
    fn omitted_footer_remains_in_original() {
        let text = "Body.\n\n## Wires\n\n- supports [[Elsewhere]]\n";
        let (mut plan, entry) = source(text);
        plan.documents[0].body_end = 5;
        let prepared = prepare(&plan.documents[0], &entry, text.as_bytes().to_vec()).unwrap();
        assert!(!prepared.tiptap_json.to_string().contains("Elsewhere"));
        assert_eq!(prepared.original, text.as_bytes());
    }
}
