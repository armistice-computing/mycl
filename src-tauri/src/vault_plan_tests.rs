use super::*;

const EXPORT: &[u8] = br#"{"format":"mycl-markdown-folder","version":1}"#;

#[test]
fn real_exporter_output_roundtrips_titles_dates_and_wire_predicates_in_plan() {
    use serde_json::json;
    let titles = [
        ("a", "He said \"hi\""),
        ("b", "A/B: [notes]"),
        ("c", "A/B: [notes]"),
    ];
    let records = titles.iter().map(|(id, title)| {
        let record = serde_json::from_value(json!({
            "documentId": id, "graphId": "g", "title": title, "body": "",
            "origin": "local", "providerId": "local-profile", "localPath": "", "rdfSubject": "",
            "createdAt": "1790000000000", "updatedAt": "1790000000000", "capabilities": [],
            "tiptapJson": {"type":"doc", "content":[{"type":"paragraph", "content":[{"type":"text", "text":"Original body."}]}]}
        })).unwrap();
        ((*id).to_string(), record)
    }).collect();
    let docs: Vec<_> = titles
        .iter()
        .map(|(id, title)| json!({"id":id, "title":title}))
        .collect();
    let wires = vec![
        json!({"id":"wire", "sourceDocumentId":"a", "targetDocumentId":"c", "predicate":"contradicts"}),
    ];
    let mut warnings = vec![];
    let exported = crate::markdown_folder_export::plan_documents(
        &[],
        &docs,
        &wires,
        &records,
        &BTreeSet::new(),
        &mut warnings,
    );
    assert!(warnings.is_empty());
    let mut files = vec![("graph.json", EXPORT)];
    files.extend(
        exported
            .iter()
            .map(|d| (d.relative_path.as_str(), d.text.as_bytes())),
    );
    let p = plan(&files);
    assert_eq!(p.documents.len(), 3);
    assert_eq!(p.export_wires.len(), 1);
    assert_eq!(p.export_wires[0].predicate, "contradicts");
    let expected = |id: &str| exported.iter().position(|d| d.document_id == id).unwrap() + 1;
    assert_eq!(
        (p.export_wires[0].source, p.export_wires[0].target),
        (expected("a"), expected("c"))
    );
    assert!(p.links.is_empty());
    assert!(p
        .issues
        .iter()
        .any(|i| i.path == exported[expected("a") - 1].relative_path
            && i.kind == "export_footer_omitted"));
    for doc in &p.documents {
        assert!(doc.metadata.created_at.is_some());
        assert!(doc.metadata.updated_at.is_some());
        assert!(!std::str::from_utf8(files[doc.entry].1).unwrap()
            [doc.metadata.body_start..doc.body_end]
            .contains("## Wires"));
    }
    assert!(p.documents.iter().any(|d| d.title == "He said \"hi\""));
}

#[test]
fn exported_wires_use_ids_deduplicate_mirrors_and_remove_exact_footer() {
    let a = b"---\nid: \"old-a\"\ntitle: \"Same\"\nwires:\n  - predicate: \"supports\"\n    to: \"Same\"\n    toId: \"old-b\"\n---\n\nA body\n\n## Wires\n\n- supports [[B|Same]]\n";
    let b = b"---\nid: \"old-b\"\ntitle: \"Same\"\nwires:\n  - predicate: \"supports\"\n    from: \"Same\"\n    fromId: \"old-a\"\n---\n\nB body\n";
    let p = plan(&[("graph.json", EXPORT), ("A.md", a), ("B.md", b)]);
    assert_eq!(p.export_wires.len(), 1);
    assert_eq!((p.export_wires[0].source, p.export_wires[0].target), (1, 2));
    assert_eq!(p.export_wires[0].predicate, "supports");
    assert!(p.links.is_empty());
    assert!(p
        .documents
        .iter()
        .all(|d| !["old-a", "old-b"].contains(&d.id.as_str())));
    let d = &p.documents[0];
    assert_eq!(&a[d.metadata.body_start..d.body_end], b"\nA body\n");
}

#[test]
fn export_block_ids_are_historical_not_fabricated_new_endpoints() {
    let a = b"---\nid: \"a\"\nwires:\n  - predicate: \"qualifies\"\n    to: \"B\"\n    toId: \"b\"\n    block: \"old-block-a\"\n    toBlock: \"old-block-b\"\n---\nA";
    let p = plan(&[
        ("graph.json", EXPORT),
        ("A.md", a),
        ("B.md", b"---\nid: \"b\"\n---\nB"),
    ]);
    assert_eq!(
        p.export_wires[0].original_source_block.as_deref(),
        Some("old-block-a")
    );
    assert_eq!(
        p.export_wires[0].original_target_block.as_deref(),
        Some("old-block-b")
    );
    assert!(p.issues.iter().any(|i| i.kind == "unmapped_export_block"));
}

#[test]
fn exported_markdown_attachments_are_not_parsed_as_documents() {
    let hostile_header = format!("---\ntitle: {}\n---", "x".repeat(70000));
    let p = plan(&[
        ("graph.json", EXPORT),
        ("attachments/id/raw.md", hostile_header.as_bytes()),
    ]);
    assert!(p.documents.is_empty());
    assert_eq!(p.attachments, vec![0, 1]);
    let ordinary = plan(&[("attachments/id/raw.md", b"note")]);
    assert_eq!(ordinary.documents.len(), 1);
}

#[test]
fn legacy_export_marker_is_supported_but_future_versions_refused() {
    let old = plan(&[
        (
            "graph.json",
            br#"{"format":"mycyl-markdown-folder","version":1}"#,
        ),
        ("attachments/a.md", b"a"),
    ]);
    assert!(old.documents.is_empty());
    let files = [(
        "graph.json",
        br#"{"format":"mycl-markdown-folder","version":2}"#.as_slice(),
    )];
    assert!(
        build(&inputs(&files), &binding(), "scan", "Import", |i| Ok(files
            [i]
            .1
            .to_vec()))
        .unwrap_err()
        .contains("version")
    );
}

#[test]
fn duplicate_export_ids_do_not_choose_a_wire_target() {
    let a = b"---\nid: \"a\"\nwires:\n  - predicate: \"supports\"\n    to: \"B\"\n    toId: \"b\"\n---\nA";
    let p = plan(&[
        ("graph.json", EXPORT),
        ("A.md", a),
        ("B.md", b"---\nid: \"b\"\n---\nB"),
        ("C.md", b"---\nid: \"b\"\n---\nC"),
    ]);
    assert!(p.export_wires.is_empty());
    assert!(p.issues.iter().any(|i| i.kind == "unresolved_export_wire"));
}

#[test]
fn user_edited_wire_footer_remains_body_content() {
    let a = b"---\nid: \"a\"\nwires:\n  - predicate: \"supports\"\n    to: \"B\"\n    toId: \"b\"\n---\nA\n\n## Wires\n\n- Personal explanation [[B]]\n";
    let p = plan(&[
        ("graph.json", EXPORT),
        ("A.md", a),
        ("B.md", b"---\nid: \"b\"\n---\nB"),
    ]);
    assert_eq!(p.documents[0].body_end, a.len());
    assert_eq!(p.links.len(), 1);
}

#[test]
fn footer_cannot_move_body_boundary_into_frontmatter() {
    let a = b"---\nid: \"a\"\nwires:\n  - predicate: \"supports\"\n    to: \"B\"\n    toId: \"b\"\n---\n## Wires\n\n- supports [[B]]\n";
    let p = plan(&[
        ("graph.json", EXPORT),
        ("A.md", a),
        ("B.md", b"---\nid: \"b\"\n---\nB"),
    ]);
    for d in p.documents {
        assert!(d.body_end >= d.metadata.body_start);
    }
}

#[test]
fn invalid_index_is_rejected_before_reader_dispatch() {
    let mut entries = inputs(&[("note.md", b"hello")]);
    entries[0].index = usize::MAX;
    assert!(build(&entries, &binding(), "scan", "Import", |_| panic!(
        "must not read"
    ))
    .unwrap_err()
    .contains("index out of range"));
}

#[test]
fn encoded_hash_is_filename_data_not_a_fragment_delimiter() {
    let p = plan(&[
        (
            "index.md",
            b"[literal](other.md#Heading) [encoded](other.md%23Heading)",
        ),
        ("other.md", b"# Heading"),
        ("other.md#Heading", b"raw"),
    ]);
    assert_eq!(p.links[0].target, Some(1));
    assert_eq!(p.links[0].fragment.as_deref(), Some("Heading"));
    assert_eq!(p.links[1].target, Some(2));
    assert!(p.links[1].fragment.is_none());
}

#[test]
fn multiline_scalar_is_retained_not_partially_promoted() {
    let source = "---\ntitle: first\n  second\n---\nbody";
    let mut issues = vec![];
    let m = metadata(source, "n.md", &mut issues).unwrap();
    assert!(m.title.is_none());
    assert!(m.front_matter.is_some());
    assert!(issues.iter().any(|i| i.kind == "retained_metadata"));
}

#[test]
fn dates_cannot_claim_negative_epoch_or_reversed_history() {
    for header in [
        "created: 1960-01-01T00:00:00Z",
        "created: 2026-01-02T00:00:00Z\nupdated: 2026-01-01T00:00:00Z",
    ] {
        let mut issues = vec![];
        let m = metadata(&format!("---\n{header}\n---\nbody"), "n.md", &mut issues).unwrap();
        assert!(m.created_at.is_none());
        assert!(m.updated_at.is_none());
        assert!(m.front_matter.is_some());
        assert!(!issues.is_empty());
    }
}

#[test]
#[ignore = "requires the explicitly downloaded pinned fixture checkouts; set DOOR1_FIXTURE_ROOT to their workspace root"]
fn external_fixture_planning_preserves_all_note_counts() {
    let root = std::path::PathBuf::from(
        std::env::var_os("DOOR1_FIXTURE_ROOT").expect("fixture workspace root"),
    );
    let inventory: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/door1-external-fixtures.json")).unwrap();
    for fixture in inventory["fixtures"].as_array().unwrap() {
        let checkout = root.join(fixture["local_checkout"].as_str().unwrap());
        let mut owned = vec![];
        for entry in fixture["inventory"].as_array().unwrap() {
            if entry["materialize"] != true {
                continue;
            }
            let bytes =
                std::fs::read(checkout.join(entry["source_path"].as_str().unwrap())).unwrap();
            assert_eq!(bytes.len() as u64, entry["bytes"].as_u64().unwrap());
            assert_eq!(
                format!("{:x}", Sha256::digest(&bytes)),
                entry["sha256"].as_str().unwrap()
            );
            owned.push((entry["path"].as_str().unwrap().to_owned(), bytes));
        }
        let files: Vec<_> = owned
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_slice()))
            .collect();
        let p = plan(&files);
        assert_eq!(
            p.documents.len() as u64,
            fixture["counts"]["ordinary_markdown"]["files"]
                .as_u64()
                .unwrap()
        );
        assert_eq!(p.documents.len() + p.attachments.len(), files.len());
        eprintln!(
            "{}: {} notes, {} attachments, {} links, {} issues",
            fixture["id"],
            p.documents.len(),
            p.attachments.len(),
            p.links.len(),
            p.issues.len()
        );
        let inputs = inputs(&files);
        let destination = crate::vault_inline_links::Destination {
            graph_id: "fixture-target".into(),
            incarnation: "fixture".into(),
            operation_id: "fixture".into(),
        };
        let image_catalog =
            crate::vault_note::images::Catalog::new(&p, &destination, |i| Ok(files[i].1.to_vec()))
                .unwrap();
        let mut resolver = crate::vault_inline_links::Resolver::new(&p);
        let mut notes: Vec<_> = p
            .documents
            .iter()
            .map(|d| {
                let original = std::str::from_utf8(files[d.entry].1).unwrap();
                let (images, warnings) = image_catalog
                    .for_note(d, &original[d.metadata.body_start..d.body_end])
                    .unwrap();
                crate::vault_note::prepare_with_images(
                    d,
                    &inputs[d.entry],
                    files[d.entry].1.to_vec(),
                    images,
                    warnings,
                )
                .unwrap()
            })
            .collect();
        for note in &notes {
            resolver.register_note(note).unwrap();
        }
        let mut paired = 0;
        let mut tagged = 0;
        let mut fragments = 0;
        let mut image_count = 0;
        for (d, note) in p.documents.iter().zip(&mut notes) {
            let original = std::str::from_utf8(files[d.entry].1).unwrap();
            if let Some(header) = &d.metadata.front_matter {
                assert_eq!(header, &original[..d.metadata.body_start]);
            }
            resolver
                .install(note, &destination)
                .unwrap_or_else(|e| panic!("{}: {}: {e}", fixture["id"], d.path));
            let doc = crate::crdt_engine::builder::ydoc_from_tiptap_json(&note.tiptap_json);
            let projection =
                crate::crdt_engine::projection::materialize_ydoc(&doc, &note.document_id);
            crate::vault_inline_links::validate_projection(
                &projection.tiptap_json,
                &projection.blocks_json,
                &note.inline_wires,
            )
            .unwrap();
            crate::vault_note::images::validate(&projection.tiptap_json, &note.images).unwrap();
            image_count += note
                .images
                .iter()
                .filter(|i| i.target_entry.is_some())
                .count();
            paired += note
                .inline_wires
                .iter()
                .filter(|w| w.predicate == "relatedTo" && w.target_block_id.is_none())
                .count();
            fragments += note
                .inline_wires
                .iter()
                .filter(|w| w.target_block_id.is_some())
                .count();
            tagged += note
                .inline_wires
                .iter()
                .filter(|w| w.predicate == "exemplifies")
                .count();
        }
        eprintln!("{}: {paired} prepared inline/wire pairs", fixture["id"]);
        eprintln!(
            "{}: {fragments} fragment pairs; {image_count} mapped images",
            fixture["id"]
        );
        assert_eq!(
            fragments,
            if fixture["id"] == "inkswell" { 1 } else { 0 },
            "fragment fixture coverage changed"
        );
        assert_eq!(
            tagged,
            p.tags
                .references
                .iter()
                .filter(|t| t.range.is_some())
                .count()
        );
        eprintln!(
            "{}: {tagged} tag pairs; {} tag hubs",
            fixture["id"],
            p.tags.hubs.len()
        );
        let expected_pairs = match fixture["id"].as_str().unwrap() {
            "para" => 58,
            "student" => 15,
            "inkswell" => 5,
            other => panic!("unexpected fixture {other}"),
        };
        let expected_hubs = match fixture["id"].as_str().unwrap() {
            "para" => 9,
            "student" => 5,
            "inkswell" => 0,
            other => panic!("unexpected fixture {other}"),
        };
        assert_eq!(p.tags.hubs.len(), expected_hubs, "tag hub coverage changed");
        assert_eq!(
            paired, expected_pairs,
            "{} inline conversion coverage changed",
            fixture["id"]
        );
        let document = |path: &str| p.documents.iter().find(|d| d.path == path).unwrap();
        let assert_link = |source: &str, raw: &str, target: &str| {
            let source = document(source).entry;
            let target = files.iter().position(|(path, _)| *path == target).unwrap();
            assert!(p
                .links
                .iter()
                .any(|l| l.source == source && l.raw == raw && l.target == Some(target)));
        };
        match fixture["id"].as_str().unwrap() {
            "para" => {
                let source = "01 Projects/Japan Trip June 2026/Japan Trip June 2026.md";
                assert_link(
                    source,
                    "Japan Itinerary Draft",
                    "01 Projects/Japan Trip June 2026/Japan Itinerary Draft.md",
                );
                assert_eq!(document(source).metadata.tags, vec!["project", "travel"]);
                assert!(document(source).metadata.created_at.is_none());
            }
            "student" => {
                assert_link(
                    "Courses/BIO101/Assignments/Cell Membrane Diagram.md",
                    "Cell Membranes",
                    "Courses/BIO101/Notes/Cell Membranes.md",
                );
                assert_link(
                    "Start Here.md",
                    "Dashboards/Home.canvas",
                    "Dashboards/Home.canvas",
                );
            }
            "inkswell" => {
                let m = &document("Writing/Codex/Mara Vance.md").metadata;
                assert_eq!(m.aliases, vec!["The lighter on Vesper Row"]);
                assert!(m.front_matter.as_ref().unwrap().contains("relationships:"));
                assert!(m.created_at.is_none());
            }
            other => panic!("unexpected fixture {other}"),
        }
    }
}

fn binding() -> Binding {
    Binding {
        principal: "human:owner".into(),
        graph: "fixture".into(),
        incarnation: "one".into(),
    }
}

fn inputs(files: &[(&str, &[u8])]) -> Vec<Input> {
    let mut admission = Admission::new(Limits::default());
    files
        .iter()
        .enumerate()
        .map(|(index, (path, bytes))| {
            let (_, kind) = admission.admit(path, false).unwrap();
            Input {
                index,
                path: (*path).into(),
                kind: if kind == Kind::Note {
                    "note"
                } else {
                    "attachment"
                }
                .into(),
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
            }
        })
        .collect()
}

fn plan(files: &[(&str, &[u8])]) -> Plan {
    build(&inputs(files), &binding(), "scan", "Imported", |i| {
        Ok(files[i].1.to_vec())
    })
    .unwrap()
}

#[test]
fn stable_ids_are_bound_to_scan_graph_and_incarnation() {
    let files = [("Folder/Note.md", b"hello".as_slice())];
    let a = plan(&files);
    let b = plan(&files);
    assert_eq!(a.documents[0].id, b.documents[0].id);
    assert_eq!(a.documents[0].parent_id, a.folders[0].id);
    assert_eq!(a.folders[0].parent_id, a.root_id);
    for changed in [
        Binding {
            graph: "other".into(),
            ..binding()
        },
        Binding {
            incarnation: "two".into(),
            ..binding()
        },
        Binding {
            principal: "other".into(),
            ..binding()
        },
    ] {
        let p = build(&inputs(&files), &changed, "scan", "Imported", |_| {
            Ok(b"hello".to_vec())
        })
        .unwrap();
        assert_ne!(a.documents[0].id, p.documents[0].id);
    }
    let p = build(
        &inputs(&files),
        &binding(),
        "other-scan",
        "Imported",
        |_| Ok(b"hello".to_vec()),
    )
    .unwrap();
    assert_ne!(a.documents[0].id, p.documents[0].id);
}

#[test]
fn integrity_and_duplicate_indices_are_rejected() {
    let files = [
        ("one.md", b"hello".as_slice()),
        ("two.md", b"world".as_slice()),
    ];
    let mut entries = inputs(&files);
    assert!(build(&entries, &binding(), "scan", "Import", |_| Ok(
        b"wrong".to_vec()
    ))
    .unwrap_err()
    .contains("integrity"));
    entries[1].index = 0;
    assert!(
        build(&entries, &binding(), "scan", "Import", |i| Ok(files[i]
            .1
            .to_vec()))
        .unwrap_err()
        .contains("duplicate")
    );
}

#[test]
fn invalid_utf8_is_not_lossily_converted() {
    let p = plan(&[("bad.md", &[0xff]), ("index.md", b"[[bad]]")]);
    assert_eq!(p.attachments, vec![0]);
    assert_eq!(p.documents.len(), 1);
    assert_eq!(p.links[0].target, Some(0));
    assert_eq!(p.links[0].target_kind.as_deref(), Some("artifact"));
    assert!(p.issues.iter().any(|i| i.kind == "invalid_utf8"));
}

#[test]
fn markdown_paths_wiki_aliases_embeds_and_self_fragments() {
    let p = plan(&[
        ("nested/index.md", b"[other](../other.md#Heading) [[Alias]] ![[photo.png]] [self](#Here)\n`[[ignored]]`\n```\n[[also ignored]]\n```"),
        ("other.md", b"---\naliases: [Alias]\n---\n# Heading"),
        ("photo.png", b"image"),
    ]);
    assert_eq!(p.links.len(), 4);
    assert_eq!(
        p.links.iter().map(|l| l.target).collect::<Vec<_>>(),
        vec![Some(1), Some(1), Some(2), Some(0)]
    );
    assert_eq!(p.links[0].fragment.as_deref(), Some("Heading"));
    assert!(p.links[2].embedded);
    assert_eq!(p.links[2].target_kind.as_deref(), Some("artifact"));
}

#[test]
fn ambiguous_wiki_names_are_not_arbitrarily_selected() {
    let p = plan(&[
        ("index.md", b"[[same]] [[a/same]]"),
        ("a/same.md", b"a"),
        ("b/same.md", b"b"),
    ]);
    assert_eq!(p.links[0].target, None);
    assert_eq!(p.links[1].target, Some(1));
    assert!(p.issues.iter().any(|i| i.kind == "ambiguous_link"));
}

#[test]
fn outside_paths_and_invalid_escapes_do_not_resolve() {
    let p = plan(&[
        (
            "index.md",
            b"[a](../secret.md) [b](%ZZ.md) [c](%2Fsecret.md)",
        ),
        ("secret.md", b"secret"),
    ]);
    assert!(p.links.iter().all(|l| l.target.is_none()));
    assert_eq!(
        p.issues
            .iter()
            .filter(|i| i.kind == "outside_source")
            .count(),
        2
    );
    assert!(p.issues.iter().any(|i| i.kind == "invalid_link"));
}

#[test]
fn frontmatter_is_retained_with_exact_body_offset() {
    let source = "---\r\ntitle: 'A title'\r\naliases: [One, 'Two']\r\ntags:\r\n  - tag\r\ncreated: 2026-01-01\r\nupdated: 2026-01-02T03:04:05Z\r\ncustom: keep\r\n---\r\nBody";
    let mut issues = vec![];
    let m = metadata(source, "n.md", &mut issues).unwrap();
    assert_eq!(m.title.as_deref(), Some("A title"));
    assert_eq!(m.aliases, vec!["One", "Two"]);
    assert_eq!(m.tags, vec!["tag"]);
    assert_eq!(&source[m.body_start..], "Body");
    assert_eq!(m.front_matter.as_deref(), Some(&source[..m.body_start]));
    assert!(m.created_at.is_none());
    assert_eq!(m.updated_at.as_deref(), Some("2026-01-02T03:04:05Z"));
    assert!(issues.iter().any(|i| i.kind == "ambiguous_date"));
    assert!(issues.iter().any(|i| i.kind == "retained_metadata"));
}

#[test]
fn duplicate_metadata_is_not_promoted() {
    let mut issues = vec![];
    let m = metadata(
        "---\ntitle: One\ntitle: Two\n---\nbody",
        "n.md",
        &mut issues,
    )
    .unwrap();
    assert!(m.title.is_none());
    assert!(m.front_matter.is_some());
    assert!(issues.iter().any(|i| i.kind == "duplicate_metadata"));
}

#[test]
fn padded_delimiters_cannot_bypass_frontmatter_bound() {
    for source in [
        format!("---{}\ntitle: title\n---\n", " ".repeat(MAX_FRONT_MATTER)),
        format!("---\ntitle: title\n---{}\n", " ".repeat(MAX_FRONT_MATTER)),
    ] {
        assert!(metadata(&source, "n.md", &mut vec![])
            .unwrap_err()
            .contains("bound"));
    }
}

#[test]
fn tag_hubs_are_generated_once_and_not_captured_notes() {
    let p = plan(&[
        (
            "A.md",
            b"---\ntags: [project/travel, Project/Travel, existing]\n---\n#project/travel #other",
        ),
        ("Existing.md", b"body"),
    ]);
    assert_eq!(p.documents.len(), 2);
    assert_eq!(p.capture_entries.len(), 2);
    assert_eq!(p.tags.hubs.len(), 2);
    assert_eq!(p.tags.references.len(), 4);
    assert_eq!(
        p.tags.references[0].target_id,
        p.tags.references[2].target_id
    );
    assert_eq!(p.tags.references[1].target_id, p.documents[1].id);
    assert_eq!(p.tags.hubs[0].title, "project/travel");
    assert!(p.tags.references[0].range.is_none());
    assert!(p.tags.references[2].range.is_some());
}

#[test]
fn tag_ambiguity_creates_hub_without_choosing_a_note() {
    let p = plan(&[
        ("A.md", b"#topic"),
        ("x/Topic.md", b"one"),
        ("y/Topic.md", b"two"),
    ]);
    assert_eq!(p.tags.hubs.len(), 1);
    assert_eq!(p.tags.references[0].target_id, p.tags.hubs[0].id);
}

#[test]
fn inline_tags_skip_code_links_escapes_html_and_numeric_fragments() {
    let text = "# Heading\n\n#real #nested/tag #日本語 #123 #12abc\n\n`#code` \\#escaped [#label](https://x/#fragment) ![#alt](pic)\n\n~~~\n#fenced\n~~~\n\n    #indented\n\n<div>\n#html\n</div>";
    let names: Vec<_> = tags::inline(text).into_iter().map(|(_, _, n)| n).collect();
    assert_eq!(names, ["real", "nested/tag", "日本語", "12abc"]);
}

#[test]
fn generated_tag_folder_avoids_existing_folder_and_warns_on_invalid_tag() {
    let p = plan(&[(
        "Tags/Note.md",
        b"---\ntags: ['two words', '#one', ONE]\n---\nbody",
    )]);
    assert_eq!(p.tags.folder_title.as_deref(), Some("Tags (2)"));
    assert_eq!(p.tags.hubs.len(), 1);
    assert_eq!(p.tags.references.len(), 1);
    assert_eq!(p.tags.warnings.len(), 1);
    assert!(p.tags.warnings[0].contains("two words"));
    assert!(p.documents[0]
        .metadata
        .front_matter
        .as_ref()
        .unwrap()
        .contains("two words"));
}

#[test]
fn tag_plan_and_warnings_participate_in_review_identity() {
    let mut p = plan(&[("A.md", b"---\ntags: [topic]\n---\nbody")]);
    let before = crate::vault_job_bridge::plan_digest(&p).unwrap();
    p.tags.hubs[0].title = "different".into();
    assert_ne!(before, crate::vault_job_bridge::plan_digest(&p).unwrap());
    let before = crate::vault_job_bridge::plan_digest(&p).unwrap();
    p.tags.warnings.push("visible fidelity warning".into());
    assert_ne!(before, crate::vault_job_bridge::plan_digest(&p).unwrap());
}

#[test]
fn inline_html_blocks_are_conservatively_excluded_without_hiding_following_tags() {
    let source =
        "#before\n\n<span> #hidden</span> #sameblock\n\n#after\n\n- <i> #list</i>\n\n#last";
    let p = plan(&[("A.md", source.as_bytes())]);
    let names: Vec<_> = p.tags.references.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["before", "after", "last"]);
    assert_eq!(p.tags.warnings.len(), 1);
    assert!(p.tags.warnings[0].contains("HTML-containing"));
}
