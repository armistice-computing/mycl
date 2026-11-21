//! Portable image sources are derived from the exact attachment inventory.
use super::*;
use std::path::{Path, PathBuf};
pub(super) struct Attachment {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}
pub(super) fn inventory(graph: &Path) -> Result<Vec<Attachment>, String> {
    let root = graph.join("artifacts");
    let mut result = vec![];
    match std::fs::symlink_metadata(&root) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err("linked artifact root cannot be exported".into())
        }
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err("artifact root is not a directory".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(e.to_string()),
    }
    for entry in std::fs::read_dir(&root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            return Err("linked artifact directory cannot be exported".into());
        }
        if !kind.is_dir() {
            continue;
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| "invalid artifact directory name")?;
        crate::ids::validate_local_id(&id, "artifact_id")?;
        let original = entry.path().join("original");
        match std::fs::symlink_metadata(&original) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err("linked artifact original directory cannot be exported".into())
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
        }
        for file in std::fs::read_dir(original).map_err(|e| e.to_string())? {
            let file = file.map_err(|e| e.to_string())?;
            let name = file
                .file_name()
                .into_string()
                .map_err(|_| "invalid artifact filename")?;
            if name == "manifest.json" {
                continue;
            }
            let kind = file.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() {
                return Err("linked artifact original cannot be exported".into());
            }
            if kind.is_file() {
                result.push(Attachment {
                    id: id.clone(),
                    name,
                    path: file.path(),
                });
            }
        }
    }
    result.sort_by(|a, b| (&a.id, &a.name).cmp(&(&b.id, &b.name)));
    Ok(result)
}
pub(super) fn paths(inventory: &[Attachment]) -> BTreeMap<String, String> {
    let mut grouped: BTreeMap<&str, Vec<&Attachment>> = BTreeMap::new();
    for file in inventory {
        grouped.entry(&file.id).or_default().push(file);
    }
    grouped
        .into_iter()
        .filter_map(|(id, files)| {
            // Ambiguous originals are exported as files, but never guessed into a link.
            if files.len() != 1 {
                return None;
            }
            Some((
                id.to_owned(),
                format!(
                    "attachments/{}/{}",
                    crate::ids::url_component(id),
                    crate::ids::url_component(&files[0].name)
                ),
            ))
        })
        .collect()
}
pub(super) fn markdown(
    record: &DocumentRecord,
    path: &str,
    files: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    fn has(node: &Value) -> bool {
        (node["type"] == "image"
            && node["attrs"]["src"].as_str().is_some_and(|s| {
                s.starts_with("mycl-artifact:") || s.starts_with("mycl-unresolved-image:")
            }))
            || node["content"].as_array().into_iter().flatten().any(has)
    }
    let content = record.tiptap_json.as_ref()?;
    if !has(content) {
        return None;
    }
    let mut content = content.clone();
    let prefix = "../".repeat(path.matches('/').count());
    fn rewrite(
        node: &mut Value,
        graph: &str,
        doc: &str,
        prefix: &str,
        files: &BTreeMap<String, String>,
        warnings: &mut Vec<String>,
    ) {
        if node["type"] == "image" {
            if let Some(src) = node["attrs"]["src"].as_str().map(str::to_owned) {
                if let Some(raw) = src.strip_prefix("mycl-unresolved-image:") {
                    if let Some(original) = crate::vault_plan::decode_url_path(raw) {
                        node["attrs"]["src"] = Value::String(original);
                    }
                } else if let Some(raw) = src.strip_prefix("mycl-artifact://") {
                    let mapped = raw.split_once('/').and_then(|(g, a)| {
                        let g = crate::vault_plan::decode_url_path(g)?;
                        let a = crate::vault_plan::decode_url_path(a)?;
                        if g != graph || crate::ids::validate_local_id(&a, "artifact_id").is_err() {
                            return None;
                        }
                        files.get(&a).map(|p| format!("{prefix}{p}"))
                    });
                    if let Some(mapped) = mapped {
                        node["attrs"]["src"] = Value::String(mapped);
                    } else {
                        warnings.push(format!("{doc}: image original is outside this export or ambiguous; local image URI retained"));
                    }
                }
            }
        }
        if let Some(children) = node["content"].as_array_mut() {
            for child in children {
                rewrite(child, graph, doc, prefix, files, warnings);
            }
        }
    }
    rewrite(
        &mut content,
        &record.graph_id,
        &record.document_id,
        &prefix,
        files,
        warnings,
    );
    Some(crate::crdt_engine::markdown_export::tiptap_json_to_markdown(&content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn record() -> DocumentRecord {
        serde_json::from_value(json!({"documentId":"d","graphId":"g","title":"Images","body":"","origin":"local","providerId":"local-profile","localPath":"","rdfSubject":"","createdAt":"1","updatedAt":"1","capabilities":[],
        "tiptapJson":{"type":"doc","content":[
            {"type":"image","attrs":{"src":"mycl-artifact://g/a","alt":"local"}},
            {"type":"image","attrs":{"src":"mycl-unresolved-image:https%3A%2F%2Fexample.invalid%2Fx.png","alt":"remote"}},
            {"type":"codeBlock","content":[{"type":"text","text":"mycl-artifact://g/a"}]}
        ]}})).unwrap()
    }
    #[test]
    fn image_export_rewrites_only_nodes_to_exact_inventory_relative_paths() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("artifacts/a/original");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::write(original.join("x y.png"), b"image").unwrap();
        std::fs::write(original.join("manifest.json"), b"{}").unwrap();
        let inventory = inventory(root.path()).unwrap();
        assert_eq!(inventory.len(), 1);
        let mapping = paths(&inventory);
        let mut warnings = vec![];
        let text = markdown(&record(), "Folder/Images.md", &mapping, &mut warnings).unwrap();
        assert!(text.contains("../attachments/a/x%20y.png"), "{text}");
        assert!(text.contains("https://example.invalid/x.png"));
        assert_eq!(text.matches("mycl-artifact://g/a").count(), 1);
        assert!(warnings.is_empty());
    }
    #[test]
    fn image_export_does_not_guess_an_ambiguous_or_cross_graph_original() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("artifacts/a/original");
        std::fs::create_dir_all(&original).unwrap();
        for name in ["a.png", "b.png"] {
            std::fs::write(original.join(name), b"x").unwrap();
        }
        assert!(paths(&inventory(root.path()).unwrap()).is_empty());
        let mut r = record();
        r.graph_id = "other".into();
        let mut warnings = vec![];
        let text = markdown(
            &r,
            "Images.md",
            &BTreeMap::from([("a".into(), "attachments/a/x.png".into())]),
            &mut warnings,
        )
        .unwrap();
        assert!(text.contains("mycl-artifact://g/a"));
        assert_eq!(warnings.len(), 1);
    }
}
