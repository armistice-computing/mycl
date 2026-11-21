use crate::{ids::content_disposition_filename, original_file_types::OriginalFileManifest};
use axum::{
    body::Body,
    http::{header, StatusCode},
    response::Response,
};
use std::collections::BTreeMap;

pub(super) fn original_file_download_response(
    manifest: &OriginalFileManifest,
    bytes: Vec<u8>,
    inline: bool,
) -> Result<Response, String> {
    let disposition = if inline { "inline" } else { "attachment" };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, manifest.mime_type.as_str())
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "{disposition}; filename=\"{}\"",
                content_disposition_filename(&manifest.filename)
            ),
        )
        .body(Body::from(bytes))
        .map_err(|error| format!("build original file response: {error}"))
}

pub(super) fn image_file_response(
    manifest: &OriginalFileManifest,
    bytes: Vec<u8>,
) -> Result<Response, String> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, manifest.mime_type.as_str())
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .header(header::CACHE_CONTROL, "private, max-age=86400")
        .header("cross-origin-resource-policy", "cross-origin")
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "inline; filename=\"{}\"",
                content_disposition_filename(&manifest.filename)
            ),
        )
        .body(Body::from(bytes))
        .map_err(|error| format!("build image file response: {error}"))
}

pub(super) fn query_bool(params: &BTreeMap<String, String>, key: &str) -> bool {
    params
        .get(key)
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            normalized == "true" || normalized == "1" || normalized == "yes"
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_file_response_sets_download_headers() {
        let manifest = OriginalFileManifest {
            filename: "quote\"name.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size_bytes: 3,
            local_path: "/tmp/quote-name.pdf".to_string(),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            sha256: None,
        };

        let response = original_file_download_response(&manifest, b"pdf".to_vec(), false).unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/pdf"
        );
        assert_eq!(response.headers().get(header::CONTENT_LENGTH).unwrap(), "3");
        assert_eq!(
            response.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"quote_name.pdf\""
        );
    }

    #[test]
    fn query_bool_accepts_common_truthy_values_only() {
        let params = BTreeMap::from([
            ("one".to_string(), "1".to_string()),
            ("yes".to_string(), " yes ".to_string()),
            ("true".to_string(), "TRUE".to_string()),
            ("false".to_string(), "false".to_string()),
        ]);

        assert!(query_bool(&params, "one"));
        assert!(query_bool(&params, "yes"));
        assert!(query_bool(&params, "true"));
        assert!(!query_bool(&params, "false"));
        assert!(!query_bool(&params, "missing"));
    }
}
