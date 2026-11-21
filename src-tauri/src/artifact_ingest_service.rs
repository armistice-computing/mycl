use crate::app_runtime::AppHandle;
use crate::{
    artifact_ingest_payloads::{
        already_ingested_update_payload, artifact_ingested_payload, artifact_upload_ingest_payload,
        editable_document_payload,
    },
    artifact_ingest_title::artifact_import_document_title,
    clock::timestamp,
    crdt_engine::upload_ingest_ops::{parser_pool_endpoint, PARSER_POOL_ENV},
    crdt_projection_flush::flush_document_projection,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    document_projection_service::hosted_document_summaries,
    hosted_navigation_projection::hosted_navigation_parts,
    json_utils::json_string,
    local_jobs::{local_job_submit_response, LocalJobProgress, LocalJobRecord, LocalJobRegistry},
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default},
    original_file_manifest_store::original_manifest_file_path,
    original_file_service::{read_artifact_original_file, read_document_original_file},
    original_file_types::OriginalFileManifest,
    paths::{existing_document_dir, existing_graph_dir},
    pdf_ingestion_commands::pdf_ingestion_pipeline_status,
    pdf_parsers::{run_docling_pdf_to_markdown, run_pymupdf4llm_pdf_to_markdown_optional_cancel},
    pdf_pipeline::{
        pdf_pipeline_job_context, PDF_DOCLING_ENGINE_ID, PDF_PYMUPDF_ENGINE_ID,
    },
    pdf_runtimes::PYMUPDF4LLM_PACKAGE_REQUIREMENT,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::sync::Arc;
use std::time::Duration;

pub(super) use crate::artifact_upload_service::mcp_local_upload_artifact;

pub(crate) async fn local_ingest_artifact(
    app: AppHandle,
    graph_id: String,
    artifact_id: String,
    mode: String,
    requested_title: Option<String>,
    mut parent_id_override: Option<String>,
    keep_ingested_link: bool,
    mark_artifact_ingested: bool,
    cancel: Option<(Arc<LocalJobRegistry>, String)>,
) -> Result<serde_json::Value, String> {
    let mode = mode.to_ascii_lowercase();
    if mode != "ingest" && mode != "import" {
        return Err("mode must be 'ingest' or 'import'".to_string());
    }
    let read_only = mode != "import";
    if read_only {
        parent_id_override = None;
    }

    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    if existing_document_dir(&graph_dir, &artifact_id).is_ok()
        && read_document_original_file(&app, &graph_id, &artifact_id).is_ok()
    {
        let editable_value = if let Some(payload) =
            already_ingested_update_payload(&artifact_id, read_only, parent_id_override.clone())
        {
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.updateDocument".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(artifact_id.clone()),
                    payload,
                },
            )
            .await?
        } else {
            serde_json::Value::Null
        };
        flush_document_projection(app.clone(), &graph_id, &artifact_id).await?;
        let document = hosted_document_summaries(&app, &graph_id)?
            .into_iter()
            .map(|summary| {
                serde_json::to_value(summary).expect("HostedDocumentSummary always serializes")
            })
            .find(|document| {
                json_string(document.get("id")).as_deref() == Some(artifact_id.as_str())
            })
            .unwrap_or_else(|| serde_json::json!({ "id": artifact_id.clone() }));
        let title = json_string(document.get("title")).unwrap_or_else(|| artifact_id.clone());
        return Ok(serde_json::json!({
            "success": true,
            "graph_id": graph_id.clone(),
            "graphId": graph_id,
            "artifact_id": artifact_id.clone(),
            "artifactId": artifact_id.clone(),
            "document_id": artifact_id.clone(),
            "documentId": artifact_id,
            "title": title,
            "mode": mode,
            "readOnly": read_only,
            "auto_ingested": true,
            "autoIngested": true,
            "already_ingested": true,
            "alreadyIngested": true,
            "document": document,
            "value": editable_value,
        }));
    }

    let (_, _, artifacts) = hosted_navigation_parts(&app, &graph_id)?;
    let artifact = artifacts
        .into_iter()
        .find(|artifact| json_string(artifact.get("id")).as_deref() == Some(artifact_id.as_str()))
        .ok_or_else(|| format!("artifact {artifact_id} not found"))?;
    // A borrowed record IS a digestion: its document already exists. A
    // second digest would make a second document pointing into the first
    // one's store and re-point the record (angel review of step 1.5).
    if crate::artifact_reference::record_is_referenced(&artifact) {
        return Err(format!(
            "artifact {artifact_id} is a reference; its bytes are outside the house and are not digested through the record (pull-into-blocks is a later step; attach the file to digest it)"
        ));
    }
    if crate::artifact_storage::record_is_borrowed(&artifact) {
        let document_id = json_string(artifact.get("ingestedDocId"))
            .ok_or_else(|| format!("artifact {artifact_id} borrows an original but names no document"))?;
        let document = hosted_document_summaries(&app, &graph_id)?
            .into_iter()
            .map(|summary| {
                serde_json::to_value(summary).expect("HostedDocumentSummary always serializes")
            })
            .find(|document| json_string(document.get("id")).as_deref() == Some(document_id.as_str()))
            .unwrap_or_else(|| serde_json::json!({ "id": document_id.clone() }));
        let title = json_string(document.get("title")).unwrap_or_else(|| document_id.clone());
        return Ok(serde_json::json!({
            "success": true,
            "graph_id": graph_id.clone(),
            "graphId": graph_id,
            "artifact_id": artifact_id.clone(),
            "artifactId": artifact_id,
            "document_id": document_id.clone(),
            "documentId": document_id,
            "title": title,
            "mode": mode,
            "readOnly": read_only,
            "auto_ingested": true,
            "autoIngested": true,
            "already_ingested": true,
            "alreadyIngested": true,
            "document": document,
            "value": serde_json::Value::Null,
        }));
    }
    let (manifest, bytes) = read_artifact_original_file(&app, &graph_id, &artifact_id)?;
    if bytes.is_empty() {
        return Err("Empty artifact file".to_string());
    }

    // Stored PDF artifacts have no parser pool on desktop/loopback/MCP
    // nodes, so document.uploadIngest would fail terminally. Route them
    // through the repaired local engines instead (mirroring the
    // pdf-accurate submission decision): Docling when it is effective and
    // prepared, otherwise the local PyMuPDF4LLM runtime as the offline
    // baseline. Bytes are parsed in place from the artifact's original
    // store — nothing is re-uploaded or re-staged. Pool-configured nodes
    // keep the uploadIngest path, as do markdown/text passthroughs.
    let mut upload_value = if is_pdf_artifact(&manifest.mime_type, &manifest.filename)
        && parser_pool_endpoint().is_none()
    {
        ingest_stored_pdf_via_local_engine(
            &app,
            &graph_dir,
            &graph_id,
            &artifact_id,
            &artifact,
            &manifest,
            &bytes,
            requested_title,
            parent_id_override,
            cancel,
        )
        .await?
    } else {
        let payload = artifact_upload_ingest_payload(
            &artifact_id,
            &artifact,
            &manifest,
            &bytes,
            requested_title,
            parent_id_override,
        );

        enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "document.uploadIngest".to_string(),
                graph_id: graph_id.clone(),
                document_id: None,
                payload,
            },
        )
        .await?
    };
    let document_id = json_string(
        upload_value
            .get("documentId")
            .or_else(|| upload_value.get("document_id")),
    )
    .ok_or_else(|| "artifact ingestion did not return a documentId".to_string())?;

    let editable_value = if read_only {
        serde_json::Value::Null
    } else {
        if let Some(object) = upload_value.as_object_mut() {
            object.insert("readOnly".to_string(), serde_json::Value::Bool(false));
        }
        enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "workspace.updateDocument".to_string(),
                graph_id: graph_id.clone(),
                document_id: Some(document_id.clone()),
                payload: editable_document_payload(&document_id),
            },
        )
        .await?
    };

    let artifact_value = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.putArtifact".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(artifact_id.clone()),
            payload: artifact_ingested_payload(
                &artifact_id,
                &artifact,
                &manifest,
                bytes.len(),
                &document_id,
                keep_ingested_link,
                mark_artifact_ingested,
            ),
        },
    )
    .await?;
    flush_document_projection(app, &graph_id, &document_id).await?;
    let document_title =
        json_string(upload_value.get("title")).unwrap_or_else(|| document_id.clone());

    Ok(serde_json::json!({
        "success": true,
        "graph_id": graph_id.clone(),
        "graphId": graph_id,
        "artifact_id": artifact_id.clone(),
        "artifactId": artifact_id,
        "document_id": document_id.clone(),
        "documentId": document_id,
        "title": document_title,
        "mode": mode,
        "readOnly": read_only,
        "auto_ingested": false,
        "autoIngested": false,
        "document": upload_value,
        "artifact": artifact_value,
        "value": editable_value,
    }))
}

fn is_pdf_artifact(mime_type: &str, filename: &str) -> bool {
    mime_type.eq_ignore_ascii_case("application/pdf")
        || filename.to_ascii_lowercase().ends_with(".pdf")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StoredPdfParseEngine {
    Docling,
    Pymupdf4llm { offline_baseline: bool },
}

/// Mirrors the repaired pdf-accurate submission decision for a stored PDF
/// artifact when no parser pool is configured: Docling when it is the
/// effective engine and prepared, otherwise the local PyMuPDF4LLM runtime
/// (flagged as the offline baseline when some other engine was effective),
/// otherwise an actionable error instead of a doomed uploadIngest.
fn stored_pdf_parse_engine(
    effective_engine_id: &str,
    docling_available: bool,
    pymupdf4llm_available: bool,
) -> Result<StoredPdfParseEngine, String> {
    if effective_engine_id == PDF_DOCLING_ENGINE_ID && docling_available {
        return Ok(StoredPdfParseEngine::Docling);
    }
    if pymupdf4llm_available {
        return Ok(StoredPdfParseEngine::Pymupdf4llm {
            offline_baseline: effective_engine_id != PDF_PYMUPDF_ENGINE_ID,
        });
    }
    Err(format!(
        "stored PDF artifact import cannot start: no parser pool is configured \
         ({PARSER_POOL_ENV} is unset) and no local PDF runtime is available \
         (Docling is not prepared and the PyMuPDF4LLM runtime is unavailable). Set \
         {PARSER_POOL_ENV}, prepare Docling, or install uv so the local runtime can \
         provision {PYMUPDF4LLM_PACKAGE_REQUIREMENT}."
    ))
}

async fn ingest_stored_pdf_via_local_engine(
    app: &AppHandle,
    graph_dir: &std::path::Path,
    graph_id: &str,
    artifact_id: &str,
    artifact: &serde_json::Value,
    manifest: &OriginalFileManifest,
    bytes: &[u8],
    requested_title: Option<String>,
    parent_id: Option<String>,
    cancel: Option<(Arc<LocalJobRegistry>, String)>,
) -> Result<serde_json::Value, String> {
    let pipeline_status = pdf_ingestion_pipeline_status(app)?;
    let docling_available = pipeline_status.docling_runtime_status.available;
    let pymupdf4llm_available = pipeline_status
        .engines
        .iter()
        .find(|engine| engine.engine_id == PDF_PYMUPDF_ENGINE_ID)
        .map(|engine| engine.available)
        .unwrap_or(false);
    let engine = stored_pdf_parse_engine(
        &pipeline_status.effective_engine_id,
        docling_available,
        pymupdf4llm_available,
    )?;
    let pipeline_context = pdf_pipeline_job_context(&pipeline_status);

    // Parse the artifact's stored original in place — the same file
    // read_artifact_original_file resolved, never a copy.
    let original_dir = crate::artifact_storage::resolve_artifact_original_dir(app, graph_id, artifact_id)?;
    let input_path = original_manifest_file_path(&original_dir, &manifest.filename)?;

    let filename = manifest.filename.clone();
    let title_hint = requested_title
        .clone()
        .filter(|value| !value.trim().is_empty());
    let parse_app = app.clone();
    let parsed = match engine {
        StoredPdfParseEngine::Docling => {
            let parse_path = input_path.clone();
            let parse_filename = filename.clone();
            let parse_title = title_hint.clone();
            tokio::task::spawn_blocking(move || {
                run_docling_pdf_to_markdown(
                    &parse_app,
                    &parse_path,
                    &parse_filename,
                    parse_title.as_deref(),
                    Duration::from_secs(600),
                    cancel
                        .as_ref()
                        .map(|(jobs, job_id)| (jobs.as_ref(), job_id.as_str())),
                )
            })
            .await
            .map_err(|error| format!("Docling task join failed: {error}"))?
        }
        StoredPdfParseEngine::Pymupdf4llm { .. } => {
            let parse_path = input_path.clone();
            let parse_filename = filename.clone();
            let parse_title = title_hint.clone();
            tokio::task::spawn_blocking(move || {
                run_pymupdf4llm_pdf_to_markdown_optional_cancel(
                    &parse_path,
                    &parse_filename,
                    parse_title.as_deref(),
                    Duration::from_secs(900),
                    cancel
                        .as_ref()
                        .map(|(jobs, job_id)| (jobs.as_ref(), job_id.as_str())),
                )
            })
            .await
            .map_err(|error| format!("PyMuPDF4LLM task join failed: {error}"))?
        }
    }?;
    let markdown = json_string(parsed.get("markdown")).unwrap_or_default();
    if markdown.trim().is_empty() {
        return Err("local PDF runtime returned empty Markdown".to_string());
    }

    let (engine_id, effective_reason, offline_baseline) = match engine {
        StoredPdfParseEngine::Docling => (
            PDF_DOCLING_ENGINE_ID,
            pipeline_context.effective_reason.clone(),
            false,
        ),
        StoredPdfParseEngine::Pymupdf4llm { offline_baseline } => {
            let reason = if offline_baseline {
                format!(
                    "{} No parser pool is configured ({PARSER_POOL_ENV} is unset), so the local PyMuPDF4LLM runtime runs as the offline baseline.",
                    pipeline_context.effective_reason
                )
            } else {
                pipeline_context.effective_reason.clone()
            };
            (PDF_PYMUPDF_ENGINE_ID, reason, offline_baseline)
        }
    };
    let title = title_hint
        .or_else(|| json_string(parsed.get("title")))
        .unwrap_or_else(|| artifact_import_document_title(artifact, None));
    let source_file = serde_json::json!({
        "storageKey": json_string(artifact.get("storageKey")).unwrap_or_else(|| {
            format!("local://artifacts/{artifact_id}/original/{}", manifest.filename)
        }),
        "originalFilename": manifest.filename,
        "mimeType": manifest.mime_type,
        "sizeBytes": bytes.len(),
        "fileType": json_string(artifact.get("fileType")),
    });
    let version = match engine_id {
        PDF_DOCLING_ENGINE_ID => parsed
            .get("doclingVersion")
            .or_else(|| parsed.get("docling_version"))
            .cloned(),
        _ => parsed
            .get("pymupdf4llmVersion")
            .or_else(|| parsed.get("pymupdf4llm_version"))
            .cloned(),
    };
    let version_key = match engine_id {
        PDF_DOCLING_ENGINE_ID => "doclingVersion",
        _ => "pymupdf4llmVersion",
    };
    let payload = serde_json::json!({
        "filename": manifest.filename,
        "mimeType": manifest.mime_type,
        "sizeBytes": bytes.len(),
        "dataBase64": BASE64_STANDARD.encode(bytes),
        "parentId": parent_id,
        "title": title,
        "markdown": markdown,
        "warnings": parsed.get("warnings").cloned().unwrap_or_else(|| serde_json::json!([])),
        "stats": parsed.get("stats").cloned().unwrap_or_else(|| serde_json::json!({})),
        "sourceFile": source_file,
        "requestedApproachId": "pdf.docling-accurate",
        "ingestionApproachId": engine_id,
        "localFallback": false,
        "ocrAvailable": engine_id == PDF_DOCLING_ENGINE_ID,
        "offlineBaseline": offline_baseline,
        "offline_baseline": offline_baseline,
        "pipelinePreferredEngineId": pipeline_context.preferred_engine_id.clone(),
        "pipelineEffectiveEngineId": engine_id,
        "pipelineEffectiveReason": effective_reason,
        version_key: version,
    });
    enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.ingestMarkdownOriginal".to_string(),
            graph_id: graph_id.to_string(),
            document_id: None,
            payload,
        },
    )
    .await
}

pub(super) async fn mcp_local_ingest_artifact(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let artifact_id = mcp_arg_string(arguments, &["artifact_id", "artifactId"])
        .ok_or_else(|| "artifact_id is required".to_string())?;
    let mode = mcp_arg_string(arguments, &["mode"]).unwrap_or_else(|| "ingest".to_string());
    let normalized_mode = mode.to_ascii_lowercase();
    if normalized_mode != "ingest" && normalized_mode != "import" {
        return Err("mode must be 'ingest' or 'import'".to_string());
    }
    let requested_title = mcp_arg_string(arguments, &["title"]);
    let parent_id = mcp_arg_string(arguments, &["parent_id", "parentId"]);

    let record = insert_mcp_artifact_ingest_job(
        app,
        jobs,
        graph_id,
        artifact_id,
        normalized_mode,
        requested_title,
        parent_id,
    )?;
    mcp_artifact_ingest_job_response(&record)
}

fn insert_mcp_artifact_ingest_job(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    graph_id: String,
    artifact_id: String,
    mode: String,
    requested_title: Option<String>,
    parent_id: Option<String>,
) -> Result<LocalJobRecord, String> {
    let detail = serde_json::json!({
        "graphId": graph_id.clone(),
        "artifactId": artifact_id.clone(),
        "mode": mode.clone(),
        "title": requested_title.clone(),
        "parentId": parent_id.clone(),
        "asyncWork": true,
        "mcpTool": "ingest_artifact",
    });
    let record = jobs.insert_queued("mcp_artifact_ingest", Some(graph_id.clone()), detail)?;
    spawn_mcp_artifact_ingest_job(
        app,
        jobs,
        record.job_id.clone(),
        graph_id,
        artifact_id,
        mode,
        requested_title,
        parent_id,
    );
    Ok(record)
}

fn spawn_mcp_artifact_ingest_job(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    job_id: String,
    graph_id: String,
    artifact_id: String,
    mode: String,
    requested_title: Option<String>,
    parent_id: Option<String>,
) {
    tokio::spawn(async move {
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        let _ = jobs.mark_running(&job_id);
        report_mcp_artifact_ingest_progress(
            &jobs,
            &job_id,
            "running",
            "Importing MCP artifact",
            1,
            2,
            serde_json::json!({
                "artifactId": artifact_id.clone(),
                "mode": mode.clone(),
            }),
        );
        let result = local_ingest_artifact(
            app,
            graph_id,
            artifact_id.clone(),
            mode,
            requested_title,
            parent_id,
            true,
            true,
            Some((jobs.clone(), job_id.clone())),
        )
        .await;
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        if result.is_ok() {
            report_mcp_artifact_ingest_progress(
                &jobs,
                &job_id,
                "complete",
                "MCP artifact import complete",
                2,
                2,
                serde_json::json!({ "artifactId": artifact_id }),
            );
        }
        let _ = jobs.finish_existing(&job_id, result, "application/json");
    });
}

fn report_mcp_artifact_ingest_progress(
    jobs: &Arc<LocalJobRegistry>,
    job_id: &str,
    phase: &str,
    message: &str,
    current: usize,
    total: usize,
    details: serde_json::Value,
) {
    let total = total.max(current);
    let percent = if total == 0 {
        0.0
    } else {
        (current as f64 / total as f64) * 100.0
    };
    let _ = jobs.update_progress(
        job_id,
        LocalJobProgress {
            phase: phase.to_string(),
            message: message.to_string(),
            current,
            total,
            percent,
            updated_at: timestamp(),
            details,
        },
    );
}

fn mcp_artifact_ingest_job_response(record: &LocalJobRecord) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(local_job_submit_response(record))
        .map_err(|error| format!("serialize MCP artifact ingest job response: {error}"))?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "jobId".to_string(),
            serde_json::Value::String(record.job_id.clone()),
        );
        object.insert("accepted".to_string(), serde_json::Value::Bool(true));
        object.insert("asyncWork".to_string(), serde_json::Value::Bool(true));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_artifact_detection_uses_mime_type_or_extension() {
        assert!(is_pdf_artifact("application/pdf", "paper.bin"));
        assert!(is_pdf_artifact("application/octet-stream", "paper.pdf"));
        assert!(is_pdf_artifact("application/octet-stream", "paper.PDF"));
        assert!(!is_pdf_artifact("text/markdown", "notes.md"));
    }

    #[test]
    fn stored_pdf_engine_prefers_effective_prepared_docling() {
        let engine = stored_pdf_parse_engine(PDF_DOCLING_ENGINE_ID, true, true)
            .expect("docling should be selected");

        assert_eq!(engine, StoredPdfParseEngine::Docling);
    }

    #[test]
    fn stored_pdf_engine_uses_pymupdf4llm_when_effective() {
        let engine = stored_pdf_parse_engine(PDF_PYMUPDF_ENGINE_ID, false, true)
            .expect("pymupdf4llm should be selected");

        assert_eq!(
            engine,
            StoredPdfParseEngine::Pymupdf4llm {
                offline_baseline: false
            }
        );
    }

    #[test]
    fn stored_pdf_engine_marks_offline_baseline_when_other_engine_was_effective() {
        for effective in [PDF_DOCLING_ENGINE_ID, "pdf.fast-text"] {
            let engine = stored_pdf_parse_engine(effective, false, true)
                .expect("pymupdf4llm baseline should be selected");

            assert_eq!(
                engine,
                StoredPdfParseEngine::Pymupdf4llm {
                    offline_baseline: true
                },
                "effective engine {effective} should fall back to the offline baseline"
            );
        }
    }

    #[test]
    fn stored_pdf_engine_errors_clearly_without_pool_or_local_runtime() {
        let error = stored_pdf_parse_engine("pdf.fast-text", false, false)
            .expect_err("no local runtime and no pool must fail");

        assert!(error.contains(PARSER_POOL_ENV), "error names the pool env var: {error}");
        assert!(
            error.contains("no local PDF runtime is available"),
            "error explains the missing runtime: {error}"
        );
        assert!(
            error.contains("uv"),
            "error offers the provisioning path: {error}"
        );
    }
}
