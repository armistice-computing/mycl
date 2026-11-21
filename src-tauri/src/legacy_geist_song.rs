//! Pure cloud-1 Song import. Never persist projections from this parser.
//! Missing source times stay empty; archive labels retain their original precision.

use crate::{
    geist_song_store::{LocalSongArchiveRecord, LocalSongCoda, LocalSongStore, LocalSongVerse},
    ids::validate_local_id,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

const META: &str = "__song_meta__:";

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Heading(u64, String),
    Paragraph(String, usize),
    Rule,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    version: u32,
    verses: Vec<Verse>,
    coda: Option<Coda>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Verse {
    text: String,
    counterpoints: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Coda {
    text: String,
    ejections_remaining: i64,
}

/// Documents are canonical TipTap `doc` trees keyed by source document ID.
pub(crate) fn parse_song(
    documents: &BTreeMap<String, Value>,
    graph_id: &str,
) -> Result<Option<LocalSongStore>, String> {
    validate_local_id(graph_id, "graph ID")?;
    let song = documents.get("geist-song");
    let past = documents.get("geist-past-songs");
    if song.is_none() && past.is_none() {
        return Ok(None);
    }
    let mut store = LocalSongStore {
        schema_version: 1,
        graph_id: graph_id.into(),
        observer: String::new(),
        verses: Vec::new(),
        coda: None,
        archives: Vec::new(),
    };
    if let Some(document) = song {
        parse_active(
            &blocks(document).map_err(|e| format!("geist-song: {e}"))?,
            &mut store,
        )
        .map_err(|e| format!("geist-song: {e}"))?;
    }
    if let Some(document) = past {
        store.archives =
            parse_archives(&blocks(document).map_err(|e| format!("geist-past-songs: {e}"))?)
                .map_err(|e| format!("geist-past-songs: {e}"))?;
    }
    // Returning an empty active store would cause read_song_store to invent the seed Song.
    if store.verses.is_empty() {
        return Err(
            "legacy Song has no active verses; native reader would fabricate a seed Song".into(),
        );
    }
    if store.verses.len() > 3 {
        return Err("legacy Song exceeds three active verses".into());
    }
    Ok(Some(store))
}

fn blocks(doc: &Value) -> Result<Vec<Block>, String> {
    if doc["type"] != "doc" {
        return Err("expected canonical TipTap doc".into());
    }
    let nodes = doc
        .get("content")
        .and_then(Value::as_array)
        .ok_or("doc.content must be an array")?;
    nodes
        .iter()
        .map(|node| {
            let kind = node["type"].as_str().ok_or("block type missing")?;
            if let Some(attrs) = node.get("attrs").filter(|v| !v.is_null()) {
                for (key, value) in attrs.as_object().ok_or("block attrs must be an object")? {
                    if !value.is_null()
                        && !matches!(key.as_str(), "id" | "data-block-id")
                        && !(key == "level" && kind == "heading")
                        && !(key == "textAlign" && value == "left")
                        && !(key == "indent" && value.as_f64() == Some(0.0))
                        && !(key == "collapsed" && value.as_bool() == Some(false))
                    {
                        return Err(format!("unsupported Song block attribute {key}"));
                    }
                }
            }
            if kind == "horizontalRule" {
                if node
                    .get("content")
                    .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
                {
                    return Err("horizontalRule has content".into());
                }
                return Ok(Block::Rule);
            }
            if !matches!(kind, "heading" | "paragraph") {
                return Err(format!("unsupported Song block {kind}"));
            }
            let mut text = String::new();
            let mut voice = None;
            let empty = Vec::new();
            let children = match node.get("content") {
                None => &empty,
                Some(v) => v.as_array().ok_or("inline content must be an array")?,
            };
            for child in children {
                if child["type"] != "text" {
                    return Err("unsupported Song inline node (expected text)".into());
                }
                let part = child["text"].as_str().ok_or("text node missing text")?;
                let mut italic = false;
                let mut bold = false;
                if let Some(marks) = child.get("marks") {
                    for mark in marks.as_array().ok_or("marks must be an array")? {
                        match mark["type"].as_str() {
                            Some("italic") if !italic => italic = true,
                            Some("bold") if !bold => bold = true,
                            _ => return Err("unsupported or duplicate Song mark".into()),
                        }
                    }
                }
                let current = match (italic, bold) {
                    (false, false) => 0,
                    (true, false) => 1,
                    (true, true) => 2,
                    _ => return Err("bold-only Song text cannot identify a legacy voice".into()),
                };
                if voice.is_some_and(|v| v != current) {
                    return Err("mixed voice marks within a paragraph".into());
                }
                voice = Some(current);
                text.push_str(part);
            }
            let voice = voice.unwrap_or(0);
            if kind == "heading" {
                if voice != 0 {
                    return Err("marked Song heading unsupported".into());
                }
                Ok(Block::Heading(
                    heading_level(&node["attrs"]["level"])?,
                    text,
                ))
            } else {
                Ok(Block::Paragraph(text, voice))
            }
        })
        .collect()
}

fn heading_level(value: &Value) -> Result<u64, String> {
    // Canonical CRDT JSON encodes numeric attributes as floats, including levels.
    let level = value
        .as_f64()
        .or_else(|| match value.as_str()? {
            "1" => Some(1.0),
            "2" => Some(2.0),
            "3" => Some(3.0),
            "4" => Some(4.0),
            "5" => Some(5.0),
            "6" => Some(6.0),
            _ => None,
        })
        .ok_or("heading level must be numeric or a legacy digit from 1 through 6")?;
    if !level.is_finite() || level.fract() != 0.0 || !(1.0..=6.0).contains(&level) {
        return Err("heading level must be an integer from 1 through 6".into());
    }
    Ok(level as u64)
}

fn native_verse(text: String, counterpoints: Vec<String>) -> LocalSongVerse {
    LocalSongVerse {
        text,
        counterpoints,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

fn parse_active(blocks: &[Block], store: &mut LocalSongStore) -> Result<(), String> {
    if blocks.first() != Some(&Block::Heading(1, "The Song".into())) {
        return Err("expected The Song level-1 title".into());
    }
    if let Some(Block::Paragraph(text, 0)) = blocks.get(1) {
        if let Some(json) = text.strip_prefix(META) {
            let meta: Meta = serde_json::from_str(json)
                .map_err(|e| format!("malformed/unsupported Song metadata: {e}"))?;
            if meta.version != 1 {
                return Err("unsupported Song metadata version".into());
            }
            let mut rendered = Vec::new();
            for (i, verse) in meta.verses.into_iter().enumerate() {
                if verse.text.trim().is_empty()
                    || verse.counterpoints.len() > 2
                    || verse.counterpoints.iter().any(|s| s.trim().is_empty())
                {
                    return Err("empty verse/voice or more than three voices".into());
                }
                if i > 0 {
                    rendered.push(Block::Rule);
                }
                rendered.push(Block::Heading(2, format!("Verse {}", -(i as i64))));
                let verse = native_verse(verse.text, verse.counterpoints);
                rendered.extend(render_verse(&verse));
                store.verses.push(verse);
            }
            if let Some(coda) = meta.coda {
                if coda.text.trim().is_empty() || !(1..=8).contains(&coda.ejections_remaining) {
                    return Err("invalid coda text/lifetime".into());
                }
                rendered.push(Block::Rule);
                rendered.push(Block::Heading(
                    3,
                    format!("Coda ({} remaining)", coda.ejections_remaining),
                ));
                rendered.extend(
                    coda.text
                        .lines()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| Block::Paragraph(s.into(), 1)),
                );
                store.coda = Some(LocalSongCoda {
                    text: coda.text,
                    ejections_remaining: coda.ejections_remaining,
                    created_at: String::new(),
                });
            }
            if blocks[2..] != rendered {
                return Err(
                    "Song metadata and rendered body disagree or contain unsupported content"
                        .into(),
                );
            }
            return Ok(());
        }
    }
    // Pre-metadata Songs are separated by horizontal rules. Do not interpret stray metadata as poetry.
    for section in blocks[1..].split(|b| *b == Block::Rule) {
        let mut body = section;
        if let Some(Block::Heading(2, label)) = body.first() {
            let index = store.verses.len();
            let roman = ["I", "II", "III"].get(index).copied().unwrap_or("");
            if label != &format!("Verse {}", -(index as i64)) && label != &format!("Verse {roman}")
            {
                return Err(format!("unsupported verse heading {label}"));
            }
            body = &body[1..];
        }
        if body == [Block::Paragraph("[awaiting composition]".into(), 0)] {
            continue;
        }
        if body
            .iter()
            .all(|b| matches!(b, Block::Paragraph(t, 0) if t.is_empty()))
        {
            continue;
        }
        if body
            .iter()
            .any(|b| matches!(b, Block::Paragraph(_, v) if *v != 0))
        {
            return Err("pre-metadata active voice attribution is ambiguous".into());
        }
        store.verses.push(parse_rendered_verse(body)?);
    }
    Ok(())
}

fn render_verse(verse: &LocalSongVerse) -> Vec<Block> {
    let voices: Vec<Vec<(&str, bool)>> = std::iter::once(&verse.text)
        .chain(&verse.counterpoints)
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    (
                        s.strip_suffix('/').unwrap_or(s).trim_end(),
                        s.ends_with('/'),
                    )
                })
                .collect()
        })
        .collect();
    let mut blocks = Vec::new();
    for line in 0..voices.iter().map(Vec::len).max().unwrap_or(0) {
        for (voice, lines) in voices.iter().enumerate() {
            if let Some((text, stanza)) = lines.get(line) {
                if !text.is_empty() {
                    blocks.push(Block::Paragraph((*text).into(), voice));
                }
                if *stanza {
                    blocks.push(Block::Paragraph(String::new(), 0));
                }
            }
        }
    }
    blocks
}

fn parse_rendered_verse(blocks: &[Block]) -> Result<LocalSongVerse, String> {
    let mut voices: [Vec<String>; 3] = Default::default();
    let mut last_voice: Option<usize> = None;
    for block in blocks {
        let Block::Paragraph(text, voice) = block else {
            return Err("unexpected heading/rule in verse body".into());
        };
        if text.contains(META) {
            return Err("stray Song metadata block".into());
        }
        if text.is_empty() {
            if let Some(v) = last_voice {
                let line: &mut String = voices[v].last_mut().unwrap();
                if !line.ends_with('/') {
                    line.push_str(" /");
                }
            }
        } else {
            voices[*voice].push(text.clone());
            last_voice = Some(*voice);
        }
    }
    if voices[0].is_empty() || (voices[1].is_empty() && !voices[2].is_empty()) {
        return Err("missing original/intermediate archive voice".into());
    }
    let text = voices[0].join("\n");
    let counterpoints = voices[1..]
        .iter()
        .filter(|v| !v.is_empty())
        .map(|v| v.join("\n"))
        .collect();
    Ok(native_verse(text, counterpoints))
}

fn parse_archives(blocks: &[Block]) -> Result<Vec<LocalSongArchiveRecord>, String> {
    let mut cursor = 0;
    if blocks.first() == Some(&Block::Heading(1, "Songs Archive".into())) {
        cursor += 1;
    }
    let mut archives = Vec::new();
    while cursor < blocks.len() {
        if blocks[cursor] == Block::Paragraph(String::new(), 0) {
            cursor += 1;
            continue;
        }
        let Block::Heading(3, heading) = &blocks[cursor] else {
            return Err("expected Archived date heading".into());
        };
        let label = heading
            .strip_prefix("Archived ")
            .ok_or("unsupported archive heading")?;
        chrono::NaiveDateTime::parse_from_str(label, "%Y-%m-%d %H:%M UTC")
            .map_err(|e| format!("invalid archive date label: {e}"))?;
        cursor += 1;
        let start = cursor;
        while cursor < blocks.len() && !matches!(&blocks[cursor], Block::Heading(..) | Block::Rule)
        {
            cursor += 1;
        }
        let verse = parse_rendered_verse(&blocks[start..cursor])?;
        archives.push(LocalSongArchiveRecord {
            archived_at: label.into(),
            verses: vec![verse],
        });
        if cursor < blocks.len() && blocks[cursor] == Block::Rule {
            cursor += 1;
        }
    }
    Ok(archives)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(text: &str, voice: usize) -> Value {
        let marks = match voice {
            0 => vec![],
            1 => vec![json!({"type":"italic"})],
            _ => vec![json!({"type":"bold"}), json!({"type":"italic"})],
        };
        if text.is_empty() {
            json!({"type":"paragraph"})
        } else {
            json!({"type":"paragraph", "content":[{"type":"text", "text":text, "marks":marks}]})
        }
    }

    fn h(level: u64, text: &str) -> Value {
        json!({"type":"heading", "attrs":{"level":level, "data-block-id":"source-id"},
            "content":[{"type":"text", "text":text}]})
    }

    fn doc(content: Vec<Value>) -> Value {
        json!({"type":"doc", "content":content})
    }
    fn rule() -> Value {
        json!({"type":"horizontalRule"})
    }

    fn basic_song() -> Value {
        doc(vec![
            h(1, "The Song"),
            h(2, "Verse 0"),
            p("newest & alive", 0),
            rule(),
            h(2, "Verse -1"),
            p("middle", 0),
            rule(),
            h(2, "Verse -2"),
            p("oldest", 0),
        ])
    }

    #[test]
    fn legacy_song_order_stanzas_and_no_fabricated_times() {
        let documents = BTreeMap::from([(
            "geist-song".into(),
            doc(vec![
                h(1, "The Song"),
                h(2, "Verse I"),
                p("one & two", 0),
                p("", 0),
                p("three /", 0),
            ]),
        )]);
        let before = documents.clone();
        let store = parse_song(&documents, "native").unwrap().unwrap();
        assert_eq!(store.verses[0].text, "one & two /\nthree /");
        assert_eq!(store.graph_id, "native");
        assert!(store.observer.is_empty());
        assert!(store.verses[0].created_at.is_empty());
        assert!(store.verses[0].updated_at.is_empty());
        assert_eq!(documents, before);
    }

    #[test]
    fn legacy_string_heading_levels_preserve_song_and_input() {
        let numeric = basic_song();
        let mut legacy = numeric.clone();
        for node in legacy["content"].as_array_mut().unwrap() {
            if node["type"] == "heading" {
                node["attrs"]["level"] =
                    json!(node["attrs"]["level"].as_u64().unwrap().to_string());
            }
        }
        let docs = BTreeMap::from([("geist-song".into(), legacy)]);
        let before = docs.clone();
        let actual = parse_song(&docs, "g").unwrap().unwrap();
        let expected = parse_song(&BTreeMap::from([("geist-song".into(), numeric)]), "g")
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(docs, before);
    }

    #[test]
    fn canonical_float_levels_and_nonsemantic_defaults_are_accepted() {
        let mut song = basic_song();
        for node in song["content"].as_array_mut().unwrap() {
            if node["type"] == "heading" {
                node["attrs"]["level"] = json!(node["attrs"]["level"].as_f64().unwrap());
            }
            if node["type"] == "paragraph" {
                node["attrs"] = json!({"indent": 0.0, "collapsed": false});
            }
        }
        let docs = BTreeMap::from([("geist-song".into(), song.clone())]);
        assert_eq!(parse_song(&docs, "g").unwrap().unwrap().verses.len(), 3);
        for invalid in [
            json!(1.5),
            json!(0.0),
            json!(-1.0),
            json!(7.0),
            json!("1.0"),
            json!("7"),
            json!(true),
            Value::Null,
        ] {
            let mut invalid_song = song.clone();
            invalid_song["content"][0]["attrs"]["level"] = invalid;
            assert!(
                parse_song(&BTreeMap::from([("geist-song".into(), invalid_song)]), "g").is_err()
            );
        }
        for (key, invalid) in [
            ("indent", json!(1.0)),
            ("indent", json!(-1)),
            ("indent", json!("0")),
            ("collapsed", json!(true)),
            ("collapsed", json!(0)),
        ] {
            let mut invalid_song = song.clone();
            invalid_song["content"][2]["attrs"][key] = invalid;
            assert!(
                parse_song(&BTreeMap::from([("geist-song".into(), invalid_song)]), "g").is_err()
            );
        }
    }

    fn metadata_song() -> Value {
        let meta = json!({"version":1,"verses":[{"text":"original /\nnext", "counterpoints":["answer", "third"]}],
            "coda":{"text":"stay", "ejections_remaining":3}});
        doc(vec![
            h(1, "The Song"),
            p(&format!("{META}{meta}"), 0),
            h(2, "Verse 0"),
            p("original", 0),
            p("", 0),
            p("answer", 1),
            p("third", 2),
            p("next", 0),
            rule(),
            h(3, "Coda (3 remaining)"),
            p("stay", 1),
        ])
    }

    #[test]
    fn metadata_preserves_exact_voices_and_coda() {
        let docs = BTreeMap::from([("geist-song".into(), metadata_song())]);
        let store = parse_song(&docs, "g").unwrap().unwrap();
        assert_eq!(store.verses[0].text, "original /\nnext");
        assert_eq!(store.verses[0].counterpoints, vec!["answer", "third"]);
        let coda = store.coda.unwrap();
        assert_eq!(coda.text, "stay");
        assert_eq!(coda.ejections_remaining, 3);
        assert!(coda.created_at.is_empty());
    }

    #[test]
    fn archives_survive_serialization_and_first_sing_ejection_shape() {
        let past = doc(vec![
            h(1, "Songs Archive"),
            h(3, "Archived 2026-09-09 21:03 UTC"),
            p("old voice", 0),
            p("answer", 1),
            p("", 0),
            p("last voice", 2),
            p("second line", 0),
            rule(),
            h(3, "Archived 2026-09-09 21:03 UTC"),
            p("another ejection", 0),
            rule(),
        ]);
        let docs = BTreeMap::from([
            ("geist-song".into(), basic_song()),
            ("geist-past-songs".into(), past),
        ]);
        let store = parse_song(&docs, "g").unwrap().unwrap();
        assert_eq!(store.archives.len(), 2);
        assert_eq!(store.archives[0].archived_at, "2026-09-09 21:03 UTC");
        assert_eq!(store.archives[0].verses[0].text, "old voice\nsecond line");
        assert_eq!(
            store.archives[0].verses[0].counterpoints,
            vec!["answer /", "last voice"]
        );
        let saved = serde_json::to_value(&store.archives).unwrap();
        let mut restored: LocalSongStore =
            serde_json::from_value(serde_json::to_value(&store).unwrap()).unwrap();
        // Same append/split operation as geist_song_service; no profile or projections are written.
        restored
            .verses
            .insert(0, native_verse("new composition".into(), vec![]));
        let ejected = restored.verses.split_off(3);
        restored.archives.push(LocalSongArchiveRecord {
            archived_at: "test event".into(),
            verses: ejected,
        });
        assert_eq!(
            serde_json::to_value(&restored.archives[..2]).unwrap(),
            saved
        );
        assert_eq!(restored.archives[2].verses[0].text, "oldest");
        let projected = crate::geist_song_projection::song_archive_blocks(&restored);
        assert!(projected.iter().any(|b| b.content == "another ejection"));
        assert!(projected.iter().any(|b| b.content == "old voice"));
    }

    #[test]
    fn absent_is_none_but_archive_only_or_empty_active_is_explicit_error() {
        assert!(parse_song(&BTreeMap::new(), "g").unwrap().is_none());
        let docs = BTreeMap::from([("geist-past-songs".into(), doc(vec![]))]);
        assert!(parse_song(&docs, "g").unwrap_err().contains("fabricate"));
        let docs = BTreeMap::from([("geist-song".into(), doc(vec![h(1, "The Song")]))]);
        assert!(parse_song(&docs, "g").is_err());
        assert!(parse_song(&BTreeMap::new(), "../g").is_err());
    }

    #[test]
    fn malformed_metadata_and_divergent_projection_are_rejected() {
        for text in [
            "__song_meta__:{",
            "__song_meta__:{\"version\":2,\"verses\":[],\"coda\":null}",
            "__song_meta__:{\"version\":1,\"verses\":[],\"actor\":\"invented\"}",
        ] {
            let docs =
                BTreeMap::from([("geist-song".into(), doc(vec![h(1, "The Song"), p(text, 0)]))]);
            assert!(parse_song(&docs, "g").is_err());
        }
        let mut song = metadata_song();
        song["content"][3] = p("unrecorded edit", 0);
        let docs = BTreeMap::from([("geist-song".into(), song)]);
        assert!(parse_song(&docs, "g").unwrap_err().contains("disagree"));
    }

    #[test]
    fn unsupported_content_and_bad_archives_fail_without_dropping_them() {
        for node in [
            json!({"type":"image","attrs":{"src":"source"}}),
            json!({"type":"paragraph","content":[{"type":"hardBreak"}]}),
            json!({"type":"paragraph","content":[{"type":"text","text":"link","marks":[{"type":"link"}]}]}),
        ] {
            let docs = BTreeMap::from([("geist-song".into(), doc(vec![h(1, "The Song"), node]))]);
            assert!(parse_song(&docs, "g").is_err());
        }
        for past in [
            doc(vec![p("unlabeled archive", 0)]),
            doc(vec![h(3, "Archived yesterday"), p("text", 0)]),
            doc(vec![
                h(3, "Archived 2026-09-09 21:03 UTC"),
                p("orphan counterpoint", 1),
            ]),
        ] {
            let docs = BTreeMap::from([
                ("geist-song".into(), basic_song()),
                ("geist-past-songs".into(), past),
            ]);
            assert!(parse_song(&docs, "g").is_err());
        }
    }
}
