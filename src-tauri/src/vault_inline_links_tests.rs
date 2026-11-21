use super::*;
use crate::vault_plan::Input;
use yrs::{updates::decoder::Decode, ReadTxn, StateVector, Transact};

#[test]
fn warning_examples_are_bounded_but_omissions_are_counted() {
    let mut warnings = Warnings::default();
    for n in 0..200 {
        warnings.push(format!("warning {n}"));
    }
    let result = warnings.finish();
    assert_eq!(result.len(), 129);
    assert!(result.last().unwrap().starts_with("72 additional"));
}

#[test]
fn review_digest_commits_to_rendered_content_and_conversion_warnings() {
    let (plan, mut note, dest) = fixture("[[B]]");
    let before =
        crate::vault_job_bridge::review_digest(&plan, std::slice::from_ref(&note)).unwrap();
    install(&plan, &mut note, &dest).unwrap();
    let after = crate::vault_job_bridge::review_digest(&plan, std::slice::from_ref(&note)).unwrap();
    assert_ne!(before, after);
    note.install_inline_links(
        note.tiptap_json.clone(),
        note.inline_wires.clone(),
        vec!["explicit fidelity warning".into()],
    )
    .unwrap();
    assert_ne!(
        after,
        crate::vault_job_bridge::review_digest(&plan, &[note]).unwrap()
    );
}

fn fixture(body: &str) -> (Plan, PreparedNote, Destination) {
    fixture_target(body, "# Target")
}
fn fixture_target(body: &str, target: &str) -> (Plan, PreparedNote, Destination) {
    let files = [
        ("A.md", body.as_bytes()),
        ("B.md", target.as_bytes()),
        ("file.bin", b"binary".as_slice()),
    ];
    let entries: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(index, (path, bytes))| Input {
            index,
            path: (*path).into(),
            kind: if path.ends_with(".md") {
                "note"
            } else {
                "attachment"
            }
            .into(),
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        })
        .collect();
    let binding = crate::vault_capture::Binding {
        graph: "source".into(),
        incarnation: "generation".into(),
        principal: "owner".into(),
    };
    let plan = crate::vault_plan::build(&entries, &binding, "scan", "Imported", |i| {
        Ok(files[i].1.to_vec())
    })
    .unwrap();
    let note =
        crate::vault_note::prepare(&plan.documents[0], &entries[0], body.as_bytes().to_vec())
            .unwrap();
    (
        plan,
        note,
        Destination {
            graph_id: "target".into(),
            incarnation: "one".into(),
            operation_id: "operation".into(),
        },
    )
}

fn reloaded(note: &PreparedNote) -> crate::crdt_engine::projection::ProjectionSnapshot {
    let doc = crate::crdt_engine::builder::ydoc_from_tiptap_json(&note.tiptap_json);
    let bytes = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let loaded = yrs::Doc::new();
    loaded
        .transact_mut()
        .apply_update(yrs::Update::decode_v1(&bytes).unwrap())
        .unwrap();
    let projection = crate::crdt_engine::projection::materialize_ydoc(&loaded, &note.document_id);
    validate_projection(
        &projection.tiptap_json,
        &projection.blocks_json,
        &note.inline_wires,
    )
    .unwrap();
    projection
}

#[test]
fn repeated_aliases_and_markdown_links_have_distinct_stable_pairs() {
    let body = "Préface [[B|the target]], then [another](B.md), then [[B]].";
    let (plan, mut a, dest) = fixture(body);
    let (_, mut b, _) = fixture(body);
    install(&plan, &mut a, &dest).unwrap();
    install(&plan, &mut b, &dest).unwrap();
    assert_eq!(a.inline_wires.len(), 3);
    assert_eq!(a.preparation_sha256, b.preparation_sha256);
    assert_eq!(
        a.inline_wires
            .iter()
            .map(|w| &w.id)
            .collect::<BTreeSet<_>>()
            .len(),
        3
    );
    let projection = reloaded(&a);
    assert_eq!(projection.body, "Préface the target, then another, then B.");
    assert_eq!(a.original, body.as_bytes());
    assert!(!contains(&a.tiptap_json, "MYCL_IMPORT_REF_"));
}

#[test]
fn code_fences_inline_code_and_external_links_are_not_rewritten() {
    let body = "`[[B]]` and [web](https://example.com)\n\n```md\n[[B]]\n```\n\n[[B]]";
    let (plan, mut note, dest) = fixture(body);
    install(&plan, &mut note, &dest).unwrap();
    assert_eq!(note.inline_wires.len(), 1);
    let p = reloaded(&note);
    assert!(p.body.contains("[[B]]"));
    assert!(contains(&p.tiptap_json, "https://example.com"));
}

#[test]
fn nested_lists_and_headings_use_actual_projected_source_blocks() {
    let (plan, mut note, dest) =
        fixture("# [[B|Heading]]\n\n- first [[B]]\n  - nested [target](B.md)");
    install(&plan, &mut note, &dest).unwrap();
    assert_eq!(note.inline_wires.len(), 3);
    let p = reloaded(&note);
    let ids: BTreeSet<_> = p
        .blocks_json
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap())
        .collect();
    assert!(note
        .inline_wires
        .iter()
        .all(|w| ids.contains(w.source_block_id.as_str())));
}

#[test]
fn artifact_atoms_preserve_file_target_through_ydoc_reload() {
    let (plan, mut note, dest) = fixture("Read [[file.bin|the file]].");
    install(&plan, &mut note, &dest).unwrap();
    assert_eq!(note.inline_wires.len(), 1);
    assert!(note.inline_wires[0].artifact);
    assert_eq!(
        note.inline_wires[0].target_id,
        crate::vault_graph_stage::artifact_id(
            &dest.graph_id,
            &dest.incarnation,
            &dest.operation_id,
            &plan.capture_entries[2]
        )
        .unwrap()
    );
    reloaded(&note);
}

#[test]
fn reference_links_and_crlf_offsets_are_supported() {
    let (plan, mut note, dest) =
        fixture("---\r\ntitle: Source\r\n---\r\nRead [that][ref].\r\n\r\n[ref]: B.md\r\n");
    install(&plan, &mut note, &dest).unwrap();
    assert_eq!(note.inline_wires.len(), 1);
    assert_eq!(reloaded(&note).body, "Read that.");
}

#[test]
fn unknown_targets_fragments_and_embedded_images_stay_unconverted() {
    let (plan, mut note, dest) = fixture("[[missing]] [[B#Heading]] ![image](file.bin)");
    let before = note.tiptap_json.clone();
    install(&plan, &mut note, &dest).unwrap();
    assert!(note.inline_wires.is_empty());
    assert_eq!(note.tiptap_json, before);
}

#[test]
fn formatted_labels_are_explicit_about_native_chip_styling() {
    let (plan, mut note, dest) = fixture("[**important**](B.md)");
    install(&plan, &mut note, &dest).unwrap();
    assert_eq!(note.inline_wires.len(), 1);
    assert!(note.warnings.iter().any(|w| w.contains("styling")));
    assert_eq!(reloaded(&note).body, "important");
}

#[test]
fn altered_atom_target_fails_pair_validation() {
    let (plan, mut note, dest) = fixture("[[B]]");
    install(&plan, &mut note, &dest).unwrap();
    let p = reloaded(&note);
    let mut content = p.tiptap_json.clone();
    content["content"][0]["content"][0]["attrs"]["targetDocId"] = json!("wrong");
    assert!(validate_projection(&content, &p.blocks_json, &note.inline_wires).is_err());
}

#[test]
fn angle_wrapped_reference_does_not_turn_into_an_html_tag() {
    let (plan, mut note, dest) = fixture("See <[[B|the type]]> here.");
    install(&plan, &mut note, &dest).unwrap();
    assert_eq!(reloaded(&note).body, "See <the type> here.");
    assert_eq!(note.inline_wires.len(), 1);
}

#[test]
#[ignore = "write a synthetic frontend contract fixture only when explicitly requested"]
fn export_click_fixture() {
    let path = std::env::var_os("VAULT_INLINE_FIXTURE_OUT").expect("fixture output path");
    let (plan, mut note, dest) = fixture("See [[B|the note]] and [the file](file.bin).");
    install(&plan, &mut note, &dest).unwrap();
    let projection = reloaded(&note);
    let value = json!({"sourceDocumentId":note.document_id,"graphId":dest.graph_id,"document":projection.tiptap_json,"wires":note.inline_wires});
    std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

#[test]
fn tags_and_links_to_same_note_keep_distinct_predicates() {
    let (plan, mut note, dest) = fixture("#B and [[B]] and #new/topic");
    install(&plan, &mut note, &dest).unwrap();
    assert_eq!(note.inline_wires.len(), 3);
    assert_eq!(note.inline_wires[0].predicate, "exemplifies");
    assert_eq!(note.inline_wires[1].predicate, "relatedTo");
    assert_eq!(note.inline_wires[2].predicate, "exemplifies");
    assert_eq!(
        note.inline_wires[0].target_id,
        note.inline_wires[1].target_id
    );
    assert_eq!(note.inline_wires[2].target_id, plan.tags.hubs[0].id);
    assert!(note.inline_wires[0].target_entry.is_none());
    assert!(note.inline_wires[1].target_entry.is_some());
}

fn fragment_notes(source: &str, target: &str) -> (Plan, Vec<PreparedNote>, Destination) {
    let (plan, note, dest) = fixture_target(source, target);
    let other = crate::vault_note::prepare(
        &plan.documents[1],
        &plan.capture_entries[1],
        target.as_bytes().to_vec(),
    )
    .unwrap();
    let mut notes = vec![note, other];
    let mut resolver = Resolver::new(&plan);
    for note in &notes {
        resolver.register_note(note).unwrap();
    }
    for note in &mut notes {
        resolver.install(note, &dest).unwrap();
    }
    (plan, notes, dest)
}

#[test]
fn fragments_map_headings_paths_slugs_blocks_and_self_links_to_real_ids() {
    let source="[[B#Introduction]] [[B#Parent#Child]] [pretty](B.md#introduction) [[B#^para-id]] [[B#^list-id]]\n\n# Here\n\n[[#Here]]";
    let target="# Introduction\n\nText **bold** ^para-id\n\n- item ^list-id\n\n# Parent\n\n## Child\n\nText";
    let (_, notes, _) = fragment_notes(source, target);
    assert_eq!(notes[0].inline_wires.len(), 6);
    let projection = reloaded(&notes[1]);
    let blocks = projection.blocks_json.as_array().unwrap();
    let ids: BTreeSet<_> = blocks.iter().filter_map(|b| b["id"].as_str()).collect();
    for wire in &notes[0].inline_wires[..5] {
        assert!(ids.contains(wire.target_block_id.as_deref().unwrap()));
    }
    assert_eq!(
        notes[0].inline_wires[0].target_block_id,
        notes[0].inline_wires[2].target_block_id
    );
    assert_ne!(
        notes[0].inline_wires[3].target_block_id,
        notes[0].inline_wires[4].target_block_id
    );
    assert_eq!(notes[0].inline_wires[5].target_id, notes[0].document_id);
    let source_projection = reloaded(&notes[0]);
    validate_projection(
        &source_projection.tiptap_json,
        &source_projection.blocks_json,
        &notes[0].inline_wires,
    )
    .unwrap();
    assert_eq!(notes[1].original, target.as_bytes());
}

#[test]
fn ambiguous_missing_code_escaped_and_standalone_markers_never_guess() {
    let source="[[B#Same]] [[B#^dup]] [[B#^code]] [[B#^escaped]] [[B#Missing]] [[B#^standalone]] [[B#Second#Same]]";
    let target="# First\n\n## Same\n\nA ^dup\n\n# Second\n\n## Same\n\nB ^dup\n\n`text ^code`\n\ntext \\^escaped\n\n^standalone";
    let (_, notes, _) = fragment_notes(source, target);
    assert_eq!(notes[0].inline_wires.len(), 1);
    assert_eq!(
        notes[0]
            .warnings
            .iter()
            .filter(|w| w.contains("no unique supported target"))
            .count(),
        6
    );
}

#[test]
fn encoded_fragments_decode_once_and_alias_labels_survive() {
    let (_, notes, _) = fragment_notes(
        "[Label](B.md#Hello%20World) [[B#Hello World|Alias]] [[B#%2548ello World]]",
        "# Hello World",
    );
    assert_eq!(notes[0].inline_wires.len(), 2);
    let content = serde_json::to_string(&notes[0].tiptap_json).unwrap();
    assert!(content.contains("Label"));
    assert!(content.contains("Alias"));
}

#[test]
fn encoded_hash_is_heading_data_not_a_hierarchy_separator() {
    let (_, notes, _) = fragment_notes(
        "[[B#A%23B]] [[B#A#B]] [literal](B.md#A%23B)",
        "# A#B\n\n# A\n\n## B",
    );
    assert_eq!(notes[0].inline_wires.len(), 3);
    assert_ne!(
        notes[0].inline_wires[0].target_block_id,
        notes[0].inline_wires[1].target_block_id
    );
    assert_eq!(
        notes[0].inline_wires[0].target_block_id,
        notes[0].inline_wires[2].target_block_id
    );
}

#[test]
#[ignore = "writes a synthetic frontend fixture only when explicitly requested"]
fn export_fragment_click_fixture() {
    let path = std::env::var_os("VAULT_INLINE_FIXTURE_OUT").expect("fixture output path");
    let (_, notes, dest) = fragment_notes(
        "See [[B#Heading|the heading]] and [[B#^marker|the passage]].",
        "# Heading\n\nBody ^marker",
    );
    let source = reloaded(&notes[0]);
    let target = reloaded(&notes[1]);
    let value = json!({"sourceDocumentId":notes[0].document_id,"targetDocumentId":notes[1].document_id,"graphId":dest.graph_id,
        "document":source.tiptap_json,"targetDocument":target.tiptap_json,"targetBlocks":target.blocks_json,"wires":notes[0].inline_wires});
    std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}
