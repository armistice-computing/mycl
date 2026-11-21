use crate::{
    ids::validate_local_id, loopback_http::loopback_error,
    multipart_pending_upload::stream_field_to_pending_upload, paths::cleanup_pending_upload_file,
    runtime_config::LOCAL_UPLOAD_MAX_BYTES,
};
use axum::{extract::Multipart, http::StatusCode, response::Response};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub(super) struct GraphImportUpload {
    pub(super) filename: String,
    pub(super) new_graph_id: String,
    pub(super) new_title: Option<String>,
    pub(super) pending_archive_path: PathBuf,
    pub(super) size_bytes: usize,
}

pub(super) struct ArchiveImportUpload {
    pub(super) filename: String,
    pub(super) folder_name: Option<String>,
    pub(super) pending_archive_path: PathBuf,
    pub(super) size_bytes: usize,
}

pub(super) async fn read_graph_import_upload(
    profile_dir: &Path,
    mut multipart: Multipart,
) -> Result<GraphImportUpload, Response> {
    let mut filename: Option<String> = None;
    let mut new_graph_id: Option<String> = None;
    let mut new_title: Option<String> = None;
    let mut pending_archive_path: Option<PathBuf> = None;
    let mut size_bytes: usize = 0;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid multipart graph import: {error}"),
                ));
            }
        };

        let name = field.name().map(str::to_string).unwrap_or_default();
        if name == "file" {
            let field_filename = field
                .file_name()
                .map(str::to_string)
                .unwrap_or_else(|| "graph.tar.gz".to_string());
            if let Some(previous_path) = pending_archive_path.take() {
                cleanup_pending_upload_file(&previous_path);
            }
            let upload = match stream_field_to_pending_upload(
                profile_dir,
                field,
                LOCAL_UPLOAD_MAX_BYTES,
                "failed to read graph import archive",
            )
            .await
            {
                Ok(upload) => upload,
                Err(error) => {
                    cleanup_optional_pending_upload(&pending_archive_path);
                    return Err(loopback_error(error.status, &error.message));
                }
            };
            if !pending_file_has_gzip_header(&upload.path).unwrap_or(false) {
                cleanup_pending_upload_file(&upload.path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    "Invalid archive format. Expected a gzip-compressed tar file (.tar.gz)",
                ));
            }
            filename = Some(field_filename);
            size_bytes = upload.bytes_written;
            pending_archive_path = Some(upload.path);
            continue;
        }

        let text = match field.text().await {
            Ok(value) => value.trim().to_string(),
            Err(error) => {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to read multipart field {name}: {error}"),
                ));
            }
        };
        if text.is_empty() {
            continue;
        }
        match name.as_str() {
            "new_graph_id" | "newGraphId" => new_graph_id = Some(text),
            "new_title" | "newTitle" => new_title = Some(text),
            _ => {}
        }
    }

    let Some(pending_archive_path) = pending_archive_path else {
        return Err(loopback_error(
            StatusCode::BAD_REQUEST,
            "missing multipart file field",
        ));
    };
    let Some(new_graph_id) = new_graph_id else {
        return Err(loopback_error(
            StatusCode::BAD_REQUEST,
            "new_graph_id is required",
        ));
    };
    if new_graph_id.len() > 128 {
        cleanup_pending_upload_file(&pending_archive_path);
        return Err(invalid_graph_id_error(None));
    }
    if let Err(error) = validate_local_id(&new_graph_id, "new_graph_id") {
        cleanup_pending_upload_file(&pending_archive_path);
        return Err(invalid_graph_id_error(Some(error)));
    }

    Ok(GraphImportUpload {
        filename: filename.unwrap_or_else(|| "graph.tar.gz".to_string()),
        new_graph_id,
        new_title,
        pending_archive_path,
        size_bytes,
    })
}

pub(super) async fn read_archive_import_upload(
    source_type: &str,
    graph_dir: &Path,
    mut multipart: Multipart,
) -> Result<ArchiveImportUpload, Response> {
    let mut filename: Option<String> = None;
    let mut folder_name: Option<String> = None;
    let mut pending_archive_path: Option<PathBuf> = None;
    let mut size_bytes: usize = 0;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid multipart import: {error}"),
                ));
            }
        };

        let name = field.name().map(str::to_string).unwrap_or_default();
        if name == "file" {
            let field_filename = field
                .file_name()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{source_type}.zip"));
            if !field_filename.to_ascii_lowercase().ends_with(".zip") {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    "File must be a ZIP archive",
                ));
            }
            if let Some(previous_path) = pending_archive_path.take() {
                cleanup_pending_upload_file(&previous_path);
            }
            let upload = match stream_field_to_pending_upload(
                graph_dir,
                field,
                LOCAL_UPLOAD_MAX_BYTES,
                "failed to read import archive",
            )
            .await
            {
                Ok(upload) => upload,
                Err(error) => {
                    cleanup_optional_pending_upload(&pending_archive_path);
                    return Err(loopback_error(error.status, &error.message));
                }
            };
            filename = Some(field_filename);
            size_bytes = upload.bytes_written;
            pending_archive_path = Some(upload.path);
            continue;
        }

        let text = match field.text().await {
            Ok(value) => value.trim().to_string(),
            Err(error) => {
                cleanup_optional_pending_upload(&pending_archive_path);
                return Err(loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to read multipart field {name}: {error}"),
                ));
            }
        };
        if text.is_empty() {
            continue;
        }
        if name == "folder_name" || name == "folderName" {
            folder_name = Some(text);
        }
    }

    let Some(pending_archive_path) = pending_archive_path else {
        return Err(loopback_error(
            StatusCode::BAD_REQUEST,
            "missing multipart file field",
        ));
    };

    Ok(ArchiveImportUpload {
        filename: filename.unwrap_or_else(|| format!("{source_type}.zip")),
        folder_name,
        pending_archive_path,
        size_bytes,
    })
}

fn invalid_graph_id_error(error: Option<String>) -> Response {
    let base = "Invalid graph ID. Must match pattern: ^[A-Za-z0-9_-]{1,128}$";
    let message = if let Some(error) = error {
        format!("{base} ({error})")
    } else {
        base.to_string()
    };
    loopback_error(StatusCode::BAD_REQUEST, &message)
}

fn cleanup_optional_pending_upload(path: &Option<PathBuf>) {
    if let Some(path) = path {
        cleanup_pending_upload_file(path);
    }
}

fn pending_file_has_gzip_header(path: &Path) -> Result<bool, String> {
    let mut file =
        File::open(path).map_err(|error| format!("open pending graph archive: {error}"))?;
    let mut header = [0_u8; 2];
    file.read_exact(&mut header)
        .map_err(|error| format!("read pending graph archive header: {error}"))?;
    Ok(header == [0x1f, 0x8b])
}
