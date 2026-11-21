use super::*;
use std::io::Write;

#[test]
fn review_budget_refuses_overflow_without_wrapping() {
    let mut remaining = 3;
    let mut writer = Budget(&mut remaining);
    writer.write_all(b"abc").unwrap();
    assert!(writer.write_all(b"d").is_err());
    assert_eq!(remaining, 0);
}

pub(crate) fn with_capture(test: impl FnOnce(AppHandle, PathBuf, Binding, String)) {
    with_capture_note(b"# Note\n\n[[diagram.bin]]", test);
}
fn with_capture_note(note: &[u8], test: impl FnOnce(AppHandle, PathBuf, Binding, String)) {
    with_capture_sources(&[("note.md", note), ("diagram.bin", b"binary bytes")], test);
}
fn with_capture_sources(
    files: &[(&str, &[u8])],
    test: impl FnOnce(AppHandle, PathBuf, Binding, String),
) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", root.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some("source".into()),
                title: "Source".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        crate::app_runtime::async_runtime::block_on(crate::vault_runtime::initialize(&app))
            .unwrap();
        let binding = crate::vault_selection::owner_binding(&app, "source").unwrap();
        let mut source = tempfile::tempfile().unwrap();
        {
            let mut zip = zip::ZipWriter::new(&mut source);
            for &(name, bytes) in files {
                zip.start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        let capture = app
            .state::<Arc<crate::vault_runtime::VaultRuntime>>()
            .store()
            .unwrap();
        let grant = capture
            .lock()
            .unwrap()
            .grant_selected(
                binding.clone(),
                source,
                crate::vault_capture::SourceKind::Zip,
                600000,
            )
            .unwrap();
        let mut ticket = capture.lock().unwrap().begin(&grant, &binding).unwrap();
        let snapshot = ticket.capture();
        let receipt = capture
            .lock()
            .unwrap()
            .finish(ticket, &binding, snapshot)
            .unwrap();
        test(
            app.clone(),
            root.path().to_path_buf(),
            binding,
            receipt.scan_id,
        );
        for graph in std::fs::read_dir(root.path().join("graphs")).unwrap() {
            let path = graph.unwrap().path();
            let _ = crate::rdf_store_service::evict_graph_store(&path);
        }
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[test]
fn captured_review_commits_once_and_does_not_recreate_deleted_destination() {
    with_capture(|app, profile, _, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let review = review(app.clone(), "source".into(), scan, "Imported".into())
                .await
                .unwrap();
            let digest = review.digest().to_string();
            let first = commit(
                app.clone(),
                review.clone(),
                digest.clone(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
            assert_eq!(first.state, State::Published);
            let target = profile
                .join("graphs")
                .join(&first.identity.destination_graph);
            let snapshot = crate::workspace_record_store::read_workspace_record(
                &target,
                &first.identity.destination_graph,
            )
            .unwrap()
            .snapshot
            .unwrap();
            assert_eq!(snapshot["documents"].as_array().unwrap().len(), 1);
            assert_eq!(snapshot["artifacts"].as_array().unwrap().len(), 1);
            assert_eq!(snapshot["wires"].as_array().unwrap().len(), 1);
            let pair = &review.notes[0].inline_wires[0];
            assert_eq!(snapshot["wires"][0]["id"], pair.id);
            assert_eq!(snapshot["wires"][0]["predicate"], "relatedTo");
            assert_eq!(snapshot["wires"][0]["sourceBlockId"], pair.source_block_id);
            assert_eq!(snapshot["wires"][0]["targetArtifactId"], pair.target_id);
            use yrs::{updates::decoder::Decode, Transact};
            let bytes = std::fs::read(crate::paths::document_ydoc_state_path(
                &target,
                &review.notes[0].document_id,
            ))
            .unwrap();
            let doc = yrs::Doc::new();
            doc.transact_mut()
                .apply_update(yrs::Update::decode_v1(&bytes).unwrap())
                .unwrap();
            let loaded = crate::crdt_engine::projection::materialize_ydoc(
                &doc,
                &review.notes[0].document_id,
            );
            crate::vault_inline_links::validate_projection(
                &loaded.tiptap_json,
                &loaded.blocks_json,
                &review.notes[0].inline_wires,
            )
            .unwrap();
            let second = commit(
                app.clone(),
                review.clone(),
                digest.clone(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
            assert_eq!(first, second);
            crate::rdf_store_service::evict_graph_store(&target).unwrap();
            std::fs::remove_dir_all(&target).unwrap();
            let after = commit(app, review, digest, Arc::new(AtomicBool::new(false)))
                .await
                .unwrap();
            assert_eq!(after.state, State::Published);
            assert!(!target.exists());
        })
    });
}
#[test]
fn stale_review_and_revoked_source_do_not_create_destination() {
    with_capture(|app, profile, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let review = review(
                app.clone(),
                "source".into(),
                scan.clone(),
                "Imported".into(),
            )
            .await
            .unwrap();
            assert!(commit(
                app.clone(),
                review.clone(),
                "wrong".into(),
                Arc::new(AtomicBool::new(false))
            )
            .await
            .is_err());
            app.state::<Arc<crate::vault_runtime::VaultRuntime>>()
                .store()
                .unwrap()
                .lock()
                .unwrap()
                .cancel(&scan, &binding)
                .unwrap();
            assert!(commit(
                app,
                review.clone(),
                review.digest().into(),
                Arc::new(AtomicBool::new(false))
            )
            .await
            .is_err());
            assert!(!profile
                .join("graphs")
                .join(crate::vault_job_bridge::destination(&binding, &scan).unwrap())
                .exists());
        })
    });
}
#[test]
fn changed_source_incarnation_refuses_old_review() {
    with_capture(|app, profile, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let review = review(
                app.clone(),
                "source".into(),
                scan.clone(),
                "Imported".into(),
            )
            .await
            .unwrap();
            let path = profile.join("graphs/source/graph.json");
            let mut graph: crate::graph_record_store::GraphRecord =
                crate::storage::read_json(&path).unwrap();
            graph.incarnation_id = Some(uuid::Uuid::new_v4().to_string());
            crate::storage::write_json(&path, &graph).unwrap();
            assert!(commit(
                app,
                review.clone(),
                review.digest().into(),
                Arc::new(AtomicBool::new(false))
            )
            .await
            .is_err());
            assert!(!profile
                .join("graphs")
                .join(crate::vault_job_bridge::destination(&binding, &scan).unwrap())
                .exists());
        })
    });
}

#[test]
fn lost_publication_ack_is_reconciled_without_source_permission_or_rewriting() {
    with_capture(|app, profile, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let review = review(
                app.clone(),
                "source".into(),
                scan.clone(),
                "Imported".into(),
            )
            .await
            .unwrap();
            let destination = crate::vault_job_bridge::destination(&binding, &scan).unwrap();
            let job = destination.strip_prefix("vault-").unwrap().to_string();
            *FAULT.lock().unwrap() = Some((job.clone(), 2));
            assert!(commit(
                app.clone(),
                review.clone(),
                review.digest().into(),
                Arc::new(AtomicBool::new(false))
            )
            .await
            .is_err());
            let target = profile.join("graphs").join(&destination);
            assert!(target.join("graph.json").exists());
            let sentinel = target.join("user-change");
            std::fs::write(&sentinel, b"keep").unwrap();
            app.state::<Arc<crate::vault_runtime::VaultRuntime>>()
                .store()
                .unwrap()
                .lock()
                .unwrap()
                .cancel(&scan, &binding)
                .unwrap();
            assert_eq!(status(app, job).await.unwrap().state, State::Published);
            assert_eq!(std::fs::read(sentinel).unwrap(), b"keep");
        })
    });
}

#[test]
fn interruption_after_intent_does_not_retry_publication_into_absence() {
    with_capture(|app, profile, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let review = review(
                app.clone(),
                "source".into(),
                scan.clone(),
                "Imported".into(),
            )
            .await
            .unwrap();
            let destination = crate::vault_job_bridge::destination(&binding, &scan).unwrap();
            let job = destination.strip_prefix("vault-").unwrap().to_string();
            *FAULT.lock().unwrap() = Some((job.clone(), 1));
            let first = commit(
                app.clone(),
                review.clone(),
                review.digest().into(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
            assert!(matches!(first.state, State::Uncertain { .. }));
            let second = commit(
                app.clone(),
                review.clone(),
                review.digest().into(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
            assert!(matches!(second.state, State::Uncertain { .. }));
            assert!(matches!(
                status(app, job).await.unwrap().state,
                State::Uncertain { .. }
            ));
            assert!(!profile.join("graphs").join(destination).exists());
        })
    });
}

#[test]
fn captured_review_registers_anchors_before_installing_fragment_pairs() {
    with_capture_note(
        b"# Note\n\nBody ^marker\n\n[[#Note]] [[#^marker]]",
        |app, profile, _, scan| {
            crate::app_runtime::async_runtime::block_on(async {
                let review = review(app.clone(), "source".into(), scan, "Imported".into())
                    .await
                    .unwrap();
                assert_eq!(review.notes[0].inline_wires.len(), 2);
                assert!(review.notes[0]
                    .inline_wires
                    .iter()
                    .all(|w| w.target_block_id.is_some()));
                let record = commit(
                    app,
                    review.clone(),
                    review.digest().into(),
                    Arc::new(AtomicBool::new(false)),
                )
                .await
                .unwrap();
                assert_eq!(record.state, State::Published);
                let workspace = crate::workspace_record_store::read_workspace_record(
                    &profile
                        .join("graphs")
                        .join(&record.identity.destination_graph),
                    &record.identity.destination_graph,
                )
                .unwrap();
                let snapshot = workspace.snapshot.unwrap();
                assert_eq!(snapshot["wires"].as_array().unwrap().len(), 2);
                assert!(snapshot["wires"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|w| w["targetBlockId"].is_string()));
            });
        },
    );
}

#[test]
fn captured_images_cannot_claim_existing_artifact_or_remote_fetch_authority() {
    with_capture_note(
        b"![private](mycl-artifact://other/private) ![remote](https://example.invalid/x.png)",
        |app, profile, _, scan| {
            crate::app_runtime::async_runtime::block_on(async {
                let review = review(app.clone(), "source".into(), scan, "Imported".into())
                    .await
                    .unwrap();
                assert_eq!(review.notes[0].images.len(), 2);
                assert!(review.notes[0].images.iter().all(
                    |i| i.target_entry.is_none() && i.src.starts_with("mycl-unresolved-image:")
                ));
                let record = commit(
                    app,
                    review.clone(),
                    review.digest().into(),
                    Arc::new(AtomicBool::new(false)),
                )
                .await
                .unwrap();
                assert_eq!(record.state, State::Published);
                assert!(profile
                    .join("graphs")
                    .join(&record.identity.destination_graph)
                    .is_dir());
            });
        },
    );
}

#[test]
fn captured_image_import_exports_portable_bytes_and_reimportable_relative_reference() {
    use std::io::Read;
    const PNG: &[u8] = include_bytes!("../icons/32x32.png");
    with_capture_sources(
        &[("note.md", b"# Note\n\n![[image.png]]"), ("image.png", PNG)],
        |app, _, _, scan| {
            let (review, record) = crate::app_runtime::async_runtime::block_on(async {
                let review = review(app.clone(), "source".into(), scan, "Imported".into())
                    .await
                    .unwrap();
                let record = commit(
                    app.clone(),
                    review.clone(),
                    review.digest().into(),
                    Arc::new(AtomicBool::new(false)),
                )
                .await
                .unwrap();
                (review, record)
            });
            let artifact = review.notes[0].images[0].artifact_id.as_ref().unwrap();
            let (manifest, bytes) = crate::original_file_service::read_artifact_original_file(
                &app,
                &record.identity.destination_graph,
                artifact,
            )
            .unwrap();
            assert_eq!(manifest.mime_type, "image/png");
            assert_eq!(bytes, PNG);
            let built = crate::markdown_folder_export::build_markdown_folder_export(
                &app,
                &record.identity.destination_graph,
            )
            .unwrap();
            let mut zip =
                zip::ZipArchive::new(std::fs::File::open(built.archive_path).unwrap()).unwrap();
            let mut files = vec![];
            for i in 0..zip.len() {
                let mut f = zip.by_index(i).unwrap();
                let name = f.name().to_owned();
                let mut bytes = vec![];
                f.read_to_end(&mut bytes).unwrap();
                files.push((name, bytes));
            }
            let image_path = format!("attachments/{artifact}/image.png");
            assert_eq!(files.iter().find(|(p, _)| p == &image_path).unwrap().1, PNG);
            let note = &files
                .iter()
                .find(|(p, _)| p.ends_with("note.md"))
                .unwrap()
                .1;
            let text = std::str::from_utf8(note).unwrap();
            assert!(text.contains(&format!("../{image_path}")), "{text}");
            assert!(!text.contains("mycl-artifact:"));
            assert!(!text.contains("token="));
            let inputs: Vec<_> = files
                .iter()
                .enumerate()
                .map(|(index, (path, bytes))| crate::vault_plan::Input {
                    index,
                    path: path.clone(),
                    kind: if path.ends_with(".md") {
                        "note"
                    } else {
                        "attachment"
                    }
                    .into(),
                    bytes: bytes.len() as u64,
                    sha256: {
                        use sha2::Digest;
                        format!("{:x}", sha2::Sha256::digest(bytes))
                    },
                })
                .collect();
            let binding = Binding {
                graph: "source".into(),
                incarnation: "new".into(),
                principal: "human:owner".into(),
            };
            let plan = crate::vault_plan::build(&inputs, &binding, "next", "Again", |i| {
                Ok(files[i].1.clone())
            })
            .unwrap();
            assert!(plan.links.iter().any(|l| l.embedded
                && l.target_kind.as_deref() == Some("artifact")
                && l.target.is_some()));
        },
    );
}

#[test]
fn modified_only_source_uses_import_clock_without_inventing_creation_history() {
    with_capture_note(
        b"---\nupdated: 2020-01-02T03:04:05Z\n---\nOld words",
        |app, profile, _, scan| {
            crate::app_runtime::async_runtime::block_on(async {
                let review = review(app.clone(), "source".into(), scan, "Imported".into())
                    .await
                    .unwrap();
                assert!(review.notes[0]
                    .warnings
                    .iter()
                    .any(|w| w.contains("system timestamps use import time")));
                let result = commit(
                    app,
                    review.clone(),
                    review.digest().into(),
                    Arc::new(AtomicBool::new(false)),
                )
                .await
                .unwrap();
                let target = profile
                    .join("graphs")
                    .join(&result.identity.destination_graph);
                let record = crate::document_record_store::read_document_record(
                    &target,
                    &target
                        .join("documents")
                        .join(&review.notes[0].document_id)
                        .join("document.json"),
                )
                .unwrap();
                assert_eq!(record.created_at, result.created_at.to_string());
                assert_eq!(record.updated_at, record.created_at);
                assert!(std::str::from_utf8(&review.notes[0].original)
                    .unwrap()
                    .contains("2020-01-02T03:04:05Z"));
            });
        },
    );
}
#[test]
fn plain_text_stays_an_attachment_without_markdown_interpretation() {
    const TEXT: &[u8] =
        b"# Heading\n\n[[Missing]] ![remote](https://example.invalid/x.png) ^marker";
    with_capture_sources(&[("literal.txt", TEXT)], |app, profile, _, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let review = review(app.clone(), "source".into(), scan, "Imported".into())
                .await
                .unwrap();
            assert!(review.notes.is_empty());
            assert_eq!(review.plan.attachments.len(), 1);
            let result = commit(
                app.clone(),
                review.clone(),
                review.digest().into(),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
            let target = profile
                .join("graphs")
                .join(&result.identity.destination_graph);
            let snapshot = crate::workspace_record_store::read_workspace_record(
                &target,
                &result.identity.destination_graph,
            )
            .unwrap()
            .snapshot
            .unwrap();
            assert!(snapshot["documents"].as_array().unwrap().is_empty());
            assert!(snapshot["wires"].as_array().unwrap().is_empty());
            let artifact = snapshot["artifacts"][0]["id"].as_str().unwrap();
            let (manifest, bytes) = crate::original_file_service::read_artifact_original_file(
                &app,
                &result.identity.destination_graph,
                artifact,
            )
            .unwrap();
            assert_eq!(manifest.mime_type, "text/plain");
            assert_eq!(bytes, TEXT);
        });
    });
}
