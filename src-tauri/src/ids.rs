use crate::clock::epoch_millis;

/// Longest title a document or graph may carry, in characters.
///
/// The old cap of 96 was a local invention the cloud never enforced, so
/// imports clipped real titles (117 and 100 characters in the 2026-09-12
/// rehearsal) while the workspace catalog kept them whole — two surfaces
/// disagreeing about a document's name. Titles are not path segments (ids
/// are), so the only reason for a cap is sanity against runaway strings.
pub(crate) const MAX_TITLE_CHARS: usize = 512;

pub(crate) fn normalize_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("title cannot be empty".to_string());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!(
            "title must be {MAX_TITLE_CHARS} characters or fewer"
        ));
    }
    Ok(title.to_string())
}

pub(crate) fn validate_local_id(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} cannot be empty"));
    }
    // Ids become path segments; "." and ".." are the only dot-forms with
    // filesystem meaning, so inner dots are safe and legacy hosted graphs
    // use them ("spread-v0.2-spec" broke the maiden import, 2026-08-29).
    if value == "." || value == ".." {
        return Err(format!("{field} contains invalid characters"));
    }
    if value.chars().all(|character| {
        character.is_ascii_alphanumeric()
            || character == '-'
            || character == '_'
            || character == '.'
    }) {
        Ok(())
    } else {
        Err(format!("{field} contains invalid characters"))
    }
}

pub(crate) fn make_graph_id(title: &str) -> String {
    format!("local-{}-{}", slugify(title), epoch_millis())
}

pub(crate) fn make_document_id(title: &str) -> String {
    format!("doc-{}-{}", slugify(title), epoch_millis())
}

fn slugify(value: &str) -> String {
    let slug = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        "item".to_string()
    } else {
        slug
    }
}

pub(crate) fn safe_filename(value: &str) -> String {
    let filename = value
        .chars()
        .map(|character| {
            if character == '/' || character == '\\' || character.is_control() {
                '_'
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_string();

    if filename.is_empty() {
        "original.bin".to_string()
    } else {
        filename
    }
}

pub(crate) fn content_disposition_filename(value: &str) -> String {
    safe_filename(value)
        .chars()
        .map(|character| {
            if character == '"' || character == '\\' || character.is_control() {
                '_'
            } else {
                character
            }
        })
        .collect()
}

pub(crate) fn extension_for_filename(filename: &str) -> Option<String> {
    let sanitized = safe_filename(filename);
    let (_, ext) = sanitized.rsplit_once('.')?;
    let ext = ext.trim().to_ascii_lowercase();
    if ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}

pub(crate) fn local_upload_mime_type_for_filename(filename: &str) -> String {
    match extension_for_filename(filename).as_deref() {
        Some("md") | Some("markdown") => "text/markdown",
        Some("html") | Some("htm") => "text/html",
        Some("txt") | Some("text") | Some("log") => "text/plain",
        Some("csv") => "text/csv",
        Some("excalidraw") => "application/vnd.excalidraw+json",
        Some("json") | Some("jsonl") | Some("jsonld") => "application/json",
        Some("xml") => "application/xml",
        Some("yaml") | Some("yml") => "application/yaml",
        Some("pdf") => "application/pdf",
        Some("epub") => "application/epub+zip",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    }
    .to_string()
}

pub(crate) fn normalize_image_mime_type(
    declared: Option<&str>,
    filename: &str,
) -> Result<String, String> {
    let declared = declared
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase());
    let ext = extension_for_filename(filename);

    let resolved = match declared.as_deref() {
        Some("image/png") => Some("image/png"),
        Some("image/jpeg") | Some("image/jpg") => Some("image/jpeg"),
        Some("image/gif") => Some("image/gif"),
        Some("image/webp") => Some("image/webp"),
        Some("image/svg+xml") | Some("image/svg") => Some("image/svg+xml"),
        Some("application/octet-stream") | None => match ext.as_deref() {
            Some("png") => Some("image/png"),
            Some("jpg") | Some("jpeg") => Some("image/jpeg"),
            Some("gif") => Some("image/gif"),
            Some("webp") => Some("image/webp"),
            Some("svg") => Some("image/svg+xml"),
            _ => None,
        },
        _ => None,
    };

    resolved.map(ToString::to_string).ok_or_else(|| {
        "Unsupported image type. Supported locally: PNG, JPG, GIF, WebP, SVG".to_string()
    })
}

pub(crate) fn url_component(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(*byte as char)
            }
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raster_artifact_mime_types_are_explicit_and_svg_is_not_promoted() {
        for (name,mime) in [("a.PNG","image/png"),("a.jpg","image/jpeg"),("a.jpeg","image/jpeg"),("a.gif","image/gif"),("a.webp","image/webp")] {
            assert_eq!(local_upload_mime_type_for_filename(name),mime);
        }
        assert_eq!(local_upload_mime_type_for_filename("a.svg"),"application/octet-stream");
    }

    #[test]
    fn validate_local_id_accepts_url_safe_ids_only() {
        assert!(validate_local_id("abc-123_X", "graph_id").is_ok());
        assert!(validate_local_id("spread-v0.2-spec", "document_id").is_ok());
        assert_eq!(
            validate_local_id("abc/123", "graph_id").expect_err("slash is unsafe"),
            "graph_id contains invalid characters"
        );
        for dot_form in [".", ".."] {
            assert_eq!(
                validate_local_id(dot_form, "document_id").expect_err("bare dot-forms are paths"),
                "document_id contains invalid characters"
            );
        }
        assert!(validate_local_id("abc\\123", "graph_id").is_err());
        assert!(validate_local_id("abc 123", "graph_id").is_err());
    }

    #[test]
    fn filenames_are_sanitized_for_local_paths_and_headers() {
        assert_eq!(safe_filename("../a\nb.pdf"), ".._a_b.pdf");
        assert_eq!(
            content_disposition_filename("quote\"name.pdf"),
            "quote_name.pdf"
        );
        assert_eq!(safe_filename(" \n "), "_");
        assert_eq!(safe_filename("   "), "original.bin");
    }

    #[test]
    fn mime_type_resolution_uses_sanitized_extension_fallbacks() {
        assert_eq!(
            local_upload_mime_type_for_filename("report.PDF"),
            "application/pdf"
        );
        assert_eq!(
            normalize_image_mime_type(Some("application/octet-stream"), "image.PNG")
                .expect("png extension fallback"),
            "image/png"
        );
        assert!(normalize_image_mime_type(Some("text/plain"), "image.png").is_err());
    }

    #[test]
    fn url_component_percent_encodes_non_unreserved_bytes() {
        assert_eq!(url_component("a b/c"), "a%20b%2Fc");
        assert_eq!(url_component("AZaz09-_.~"), "AZaz09-_.~");
    }
}
