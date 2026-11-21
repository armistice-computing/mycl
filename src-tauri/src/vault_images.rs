//! Captured raster images use stable artifact identities, never tokens or paths.
use crate::{
    vault_inline_links::Destination,
    vault_plan::{Document, Plan},
};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Image {
    /// Byte range relative to the original note body, not rewritten Markdown.
    pub start: usize,
    pub end: usize,
    pub target_entry: Option<usize>,
    pub artifact_id: Option<String>,
    pub src: String,
    pub mime: Option<String>,
    pub markdown: String,
}
struct Target {
    artifact_id: String,
    src: String,
    mime: String,
}
pub(crate) struct Catalog<'a> {
    links: BTreeMap<usize, BTreeMap<usize, &'a crate::vault_plan::Link>>,
    targets: BTreeMap<usize, Result<Target, String>>,
}
pub(crate) fn raster_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}
pub(crate) fn src(graph: &str, artifact: &str) -> String {
    format!(
        "mycl-artifact://{}/{}",
        crate::ids::url_component(graph),
        crate::ids::url_component(artifact)
    )
}
impl<'a> Catalog<'a> {
    pub(crate) fn new(
        plan: &'a Plan,
        destination: &Destination,
        mut read: impl FnMut(usize) -> Result<Vec<u8>, String>,
    ) -> Result<Self, String> {
        let entries: BTreeMap<_, _> = plan.capture_entries.iter().map(|e| (e.index, e)).collect();
        let mut links: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
        let mut targets = BTreeMap::new();
        for link in &plan.links {
            if !link.embedded {
                continue;
            }
            links
                .entry(link.source)
                .or_default()
                .insert(link.source_offset, link);
            if link.fragment.is_some() || link.target_kind.as_deref() != Some("artifact") {
                continue;
            }
            let Some(index) = link.target else {
                continue;
            };
            if targets.contains_key(&index) {
                continue;
            }
            let entry = entries
                .get(&index)
                .ok_or("image target missing from capture")?;
            let mime = crate::ids::local_upload_mime_type_for_filename(&entry.path);
            let allowed = matches!(
                mime.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            );
            let value = if !allowed {
                Err("not a supported raster image".into())
            } else if entry.bytes == 0
                || entry.bytes > crate::runtime_config::LOCAL_IMAGE_UPLOAD_MAX_BYTES as u64
            {
                Err("image exceeds the 10 MiB display bound or is empty".into())
            } else {
                let bytes = read(index)?;
                if bytes.len() as u64 != entry.bytes
                    || format!("{:x}", Sha256::digest(&bytes)) != entry.sha256
                {
                    return Err("captured image integrity mismatch".into());
                }
                if raster_mime(&bytes) != Some(mime.as_str()) {
                    Err("image signature does not match its filename type".into())
                } else {
                    let artifact_id = crate::vault_graph_stage::artifact_id(
                        &destination.graph_id,
                        &destination.incarnation,
                        &destination.operation_id,
                        entry,
                    )?;
                    Ok(Target {
                        src: src(&destination.graph_id, &artifact_id),
                        artifact_id,
                        mime,
                    })
                }
            };
            targets.insert(index, value);
        }
        Ok(Self { links, targets })
    }
    pub(crate) fn for_note(
        &self,
        document: &Document,
        body: &str,
    ) -> Result<(Vec<Image>, Vec<String>), String> {
        let mut images = vec![];
        let mut warnings = vec![];
        let mut omitted = 0usize;
        let mut warn = |message: String| {
            if warnings.len() < 128 {
                warnings.push(message);
            } else {
                omitted += 1;
            }
        };
        let mut pending: Option<(usize, usize, String, String, String)> = None;
        for (event, range) in Parser::new_ext(
            body,
            crate::crdt_engine::content_parse::md_options()
                | pulldown_cmark::Options::ENABLE_WIKILINKS,
        )
        .into_offset_iter()
        {
            match event {
                Event::Start(Tag::Image {
                    dest_url, title, ..
                }) => {
                    pending = Some((
                        range.start,
                        range.end,
                        dest_url.into_string(),
                        String::new(),
                        title.into_string(),
                    ))
                }
                Event::Text(t) | Event::Code(t) => {
                    if let Some((_, _, _, label, _)) = &mut pending {
                        label.push_str(&t);
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some((_, _, _, label, _)) = &mut pending {
                        label.push(' ');
                    }
                }
                Event::End(TagEnd::Image) => {
                    let Some((start, end, raw, mut label, mut title)) = pending.take() else {
                        continue;
                    };
                    if label.len() > 4096 || title.len() > 4096 {
                        warn(format!(
                            "Image label/title at byte {} shortened for display; original retained",
                            document.metadata.body_start + start
                        ));
                        label = label.chars().take(1024).collect();
                        title = title.chars().take(1024).collect();
                    }
                    let offset = document.metadata.body_start + start;
                    let link = self.links.get(&document.entry).and_then(|m| m.get(&offset));
                    let target = link
                        .filter(|l| l.raw == raw && l.fragment.is_none())
                        .and_then(|l| l.target)
                        .and_then(|i| self.targets.get(&i).map(|t| (i, t)));
                    let (index, artifact_id, src, mime) = match target {
                        Some((index, Ok(target))) => (
                            Some(index),
                            Some(target.artifact_id.clone()),
                            target.src.clone(),
                            Some(target.mime.clone()),
                        ),
                        other => {
                            let reason = match other {
                                Some((_, Err(reason))) => reason.as_str(),
                                _ => "no supported captured target (no external fetch)",
                            };
                            warn(format!(
                                "Image at byte {offset} retained without display mapping: {reason}"
                            ));
                            (
                                None,
                                None,
                                format!(
                                    "mycl-unresolved-image:{}",
                                    crate::ids::url_component(&raw)
                                ),
                                None,
                            )
                        }
                    };
                    let escape = |s: &str| {
                        s.replace('\\', "\\\\")
                            .replace('[', "\\[")
                            .replace(']', "\\]")
                            .replace(['\r', '\n'], " ")
                    };
                    let title = title
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"")
                        .replace(['\r', '\n'], " ");
                    let title = if title.is_empty() {
                        String::new()
                    } else {
                        format!(" \"{title}\"")
                    };
                    images.push(Image {
                        start,
                        end,
                        target_entry: index,
                        artifact_id,
                        src: src.clone(),
                        mime,
                        markdown: format!("![{}]({}{title})", escape(&label), src),
                    });
                }
                _ => {}
            }
        }
        if omitted > 0 {
            warnings.push(format!(
                "{omitted} additional image warnings omitted; originals retained"
            ));
        }
        Ok((images, warnings))
    }
}

pub(crate) fn rewrite(
    body: &str,
    images: &[Image],
    extra: impl IntoIterator<Item = (usize, usize, String)>,
) -> Result<String, String> {
    let mut edits: Vec<_> = images
        .iter()
        .map(|i| (i.start, i.end, i.markdown.clone()))
        .chain(extra)
        .collect();
    edits.sort_by_key(|e| e.0);
    let mut output = String::new();
    let mut at = 0;
    for (start, end, text) in edits {
        if end < start {
            return Err("invalid image/source range".into());
        }
        output.push_str(
            body.get(at..start)
                .ok_or("overlapping image/source transformations")?,
        );
        output.push_str(&text);
        at = end;
        if output.len() > 32 * 1024 * 1024 {
            return Err("image rendering preparation exceeds bound".into());
        }
    }
    output.push_str(body.get(at..).ok_or("invalid image/source end")?);
    if output.len() > 32 * 1024 * 1024 {
        return Err("image rendering preparation exceeds bound".into());
    }
    Ok(output)
}

pub(crate) fn validate(content: &serde_json::Value, images: &[Image]) -> Result<usize, String> {
    let mut expected = BTreeMap::new();
    for image in images {
        *expected.entry(image.src.as_str()).or_insert(0usize) += 1;
    }
    fn visit(
        node: &serde_json::Value,
        remaining: &mut BTreeMap<&str, usize>,
    ) -> Result<(), String> {
        if node["type"] == "image" {
            let src = node["attrs"]["src"].as_str().ok_or("image has no source")?;
            let count = remaining.get_mut(src).ok_or("unreviewed image source")?;
            *count = count
                .checked_sub(1)
                .ok_or("unreviewed extra image occurrence")?;
        }
        for child in node["content"].as_array().into_iter().flatten() {
            visit(child, remaining)?;
        }
        Ok(())
    }
    visit(content, &mut expected)?;
    Ok(expected.values().sum())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault_note::{prepare_with_images, PreparedNote};
    use serde_json::json;
    const PNG: &[u8] = include_bytes!("../icons/32x32.png");
    fn fixture(body: &str) -> (Plan, Vec<PreparedNote>, Destination, Vec<Vec<u8>>) {
        let files = vec![
            body.as_bytes().to_vec(),
            b"# Target\n\nBody ^target".to_vec(),
            PNG.to_vec(),
            b"<svg>not a png</svg>".to_vec(),
        ];
        let entries: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(index, bytes)| crate::vault_plan::Input {
                index,
                path: ["A.md", "B.md", "pic.png", "wrong.png"][index].into(),
                kind: if index < 2 { "note" } else { "attachment" }.into(),
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
            })
            .collect();
        let binding = crate::vault_capture::Binding {
            graph: "source".into(),
            incarnation: "source-lifetime".into(),
            principal: "human:owner".into(),
        };
        let plan = crate::vault_plan::build(&entries, &binding, "scan", "Imported", |i| {
            Ok(files[i].clone())
        })
        .unwrap();
        let destination = Destination {
            graph_id: "image-vault".into(),
            incarnation: "3be81b55-2edf-4a9c-8090-e418eb1b66b7".into(),
            operation_id: "image-job".into(),
        };
        let catalog = Catalog::new(&plan, &destination, |i| Ok(files[i].clone())).unwrap();
        let mut notes = vec![];
        for d in &plan.documents {
            let source = std::str::from_utf8(&files[d.entry]).unwrap();
            let (images, warnings) = catalog
                .for_note(d, &source[d.metadata.body_start..d.body_end])
                .unwrap();
            notes.push(
                prepare_with_images(
                    d,
                    &entries[d.entry],
                    files[d.entry].clone(),
                    images,
                    warnings,
                )
                .unwrap(),
            );
        }
        let mut resolver = crate::vault_inline_links::Resolver::new(&plan);
        for note in &notes {
            resolver.register_note(note).unwrap();
        }
        for note in &mut notes {
            resolver.install(note, &destination).unwrap();
        }
        (plan, notes, destination, files)
    }
    #[test]
    fn wiki_and_markdown_images_share_one_artifact_and_do_not_shift_fragment_targets() {
        let source="# Top\n\nbefore ![[pic.png]] after ^anchor\n\n[[#^anchor]] [[B#Target]]\n\n![caption](pic.png \"tooltip\")";
        let (plan, notes, destination, files) = fixture(source);
        let note = &notes[0];
        assert_eq!(note.images.len(), 2);
        assert_eq!(note.images[0].artifact_id, note.images[1].artifact_id);
        assert_eq!(note.inline_wires.len(), 2);
        assert_eq!(validate(&note.tiptap_json, &note.images).unwrap(), 0);
        assert_eq!(note.original, source.as_bytes());
        assert!(note.images.iter().all(
            |i| i.src.starts_with("mycl-artifact://image-vault/") && !i.src.contains("token=")
        ));
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("graphs")).unwrap();
        let target = root.path().join("graphs/image-vault");
        let graph=serde_json::from_value(json!({"graphId":destination.graph_id,"title":"Images","status":"active","origin":"local","providerId":"local-profile","localPath":crate::storage::display_path(&target),"createdAt":"1790000000000","updatedAt":"1790000000000","incarnationId":destination.incarnation,"createdByOperationId":destination.operation_id,"capabilities":[]})).unwrap();
        crate::vault_graph_stage::assemble_with_files(
            root.path(),
            graph,
            &plan,
            &notes,
            &plan.capture_entries[2..],
            |i| Ok(std::io::Cursor::new(files[i].clone())),
            |_| Ok(()),
        )
        .unwrap()
        .publish(|| Ok(()))
        .unwrap();
        let workspace =
            crate::workspace_record_store::read_workspace_record(&target, "image-vault")
                .unwrap()
                .snapshot
                .unwrap();
        assert_eq!(workspace["artifacts"].as_array().unwrap().len(), 2);
        assert_eq!(std::fs::read_dir(target.join("images")).unwrap().count(), 0);
        let record = crate::document_record_store::read_document_record(
            &target,
            &target
                .join("documents")
                .join(&note.document_id)
                .join("document.json"),
        )
        .unwrap();
        assert_eq!(
            validate(record.tiptap_json.as_ref().unwrap(), &note.images).unwrap(),
            0
        );
    }
    #[test]
    fn unsupported_remote_forged_and_bad_signature_sources_are_inert() {
        let (_,notes,_,_)=fixture("![remote](https://example.invalid/pic.png) ![forged](mycl-artifact://other/private) ![[missing.png]] ![[wrong.png]]");
        assert_eq!(notes[0].images.len(), 4);
        assert!(notes[0]
            .images
            .iter()
            .all(|i| i.target_entry.is_none() && i.src.starts_with("mycl-unresolved-image:")));
        assert_eq!(notes[0].warnings.len(), 4);
        assert_eq!(
            validate(&notes[0].tiptap_json, &notes[0].images).unwrap(),
            0
        );
        assert!(notes[0].images[0].src.contains("https%3A"));
    }
    #[test]
    fn nested_link_image_retains_outer_relation_without_overlapping_edits() {
        let (_, notes, _, _) = fixture("[![caption](pic.png)](B.md)");
        assert_eq!(notes[0].images.len(), 1);
        assert!(notes[0].images[0].target_entry.is_some());
        assert!(notes[0].inline_wires.is_empty());
        assert_eq!(
            validate(&notes[0].tiptap_json, &notes[0].images).unwrap(),
            0
        );
        assert!(notes[0].warnings.iter().any(|w| w.contains("nested")));
    }
    #[test]
    fn raster_signature_check_is_not_a_decoder_and_rejects_other_types() {
        for (bytes, mime) in [
            (b"\x89PNG\r\n\x1a\n".as_slice(), "image/png"),
            (b"\xff\xd8\xff".as_slice(), "image/jpeg"),
            (b"GIF89a".as_slice(), "image/gif"),
            (b"RIFFxxxxWEBP".as_slice(), "image/webp"),
        ] {
            assert_eq!(raster_mime(bytes), Some(mime));
        }
        assert_eq!(raster_mime(b"<svg></svg>"), None);
        assert_eq!(raster_mime(b"<html></html>"), None);
    }
    #[test]
    fn publisher_rechecks_signature_even_for_constructed_preparation() {
        let (plan, mut notes, destination, files) = fixture("![[wrong.png]]");
        let mut image = notes[0].images[0].clone();
        let id = crate::vault_graph_stage::artifact_id(
            &destination.graph_id,
            &destination.incarnation,
            &destination.operation_id,
            &plan.capture_entries[3],
        )
        .unwrap();
        image.target_entry = Some(3);
        image.artifact_id = Some(id.clone());
        image.mime = Some("image/png".into());
        image.src = src(&destination.graph_id, &id);
        image.markdown = format!("![wrong]({})", image.src);
        notes[0] = prepare_with_images(
            &plan.documents[0],
            &plan.capture_entries[0],
            files[0].clone(),
            vec![image],
            vec![],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("graphs")).unwrap();
        let target = root.path().join("graphs/image-vault");
        let graph=serde_json::from_value(json!({"graphId":destination.graph_id,"title":"Images","status":"active","origin":"local","providerId":"local-profile","localPath":crate::storage::display_path(&target),"createdAt":"1790000000000","updatedAt":"1790000000000","incarnationId":destination.incarnation,"createdByOperationId":destination.operation_id,"capabilities":[]})).unwrap();
        let result = crate::vault_graph_stage::assemble_with_files(
            root.path(),
            graph,
            &plan,
            &notes,
            &plan.capture_entries[2..],
            |i| Ok(std::io::Cursor::new(files[i].clone())),
            |_| Ok(()),
        );
        assert!(matches!(result,Err(ref e) if e.contains("signature")));
        assert!(!target.exists());
    }
    #[test]
    fn code_is_literal_and_heading_images_report_alt_text_fallback() {
        let (_, notes, _, _) =
            fixture("`![[pic.png]]`\n\n```\n![code](pic.png)\n```\n\n# ![[pic.png]]");
        assert_eq!(notes[0].images.len(), 1);
        assert_eq!(
            validate(&notes[0].tiptap_json, &notes[0].images).unwrap(),
            1
        );
        assert!(notes[0].warnings.iter().any(|w| w.contains("alt text")));
    }
    #[test]
    #[ignore = "exports synthetic image rendering fixture on explicit request"]
    fn export_image_fixture() {
        let (_, notes, destination, _) =
            fixture("![[pic.png]]\n\n![remote](https://example.invalid/pic.png)");
        let doc = crate::crdt_engine::builder::ydoc_from_tiptap_json(&notes[0].tiptap_json);
        let projection =
            crate::crdt_engine::projection::materialize_ydoc(&doc, &notes[0].document_id);
        let value = json!({"graphId":destination.graph_id,"sourceDocumentId":notes[0].document_id,"document":projection.tiptap_json,"images":notes[0].images,"pngFixture":"garden-epsilon-adoption/src-tauri/icons/32x32.png"});
        let path = std::env::var_os("VAULT_IMAGE_FIXTURE_OUT").expect("fixture output path");
        std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }
}
