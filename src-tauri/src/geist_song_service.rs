use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    geist_song_store::{
        past_songs_doc_id, persist_song_store, read_song_store, read_song_store_for,
        song_music_json, LocalSongArchiveRecord, LocalSongCoda, LocalSongVerse,
        CODA_EJECTION_LIFETIME, MAX_SONG_LINES, MAX_SONG_VERSES, MAX_SONG_VOICES,
    },
    graph_service::touch_graph_updated_at,
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default},
    paths::existing_graph_dir,
    profile_service::touch_profile_updated_at,
};

pub(super) fn mcp_local_song_summary(app: &AppHandle, graph_id: &str) -> serde_json::Value {
    existing_graph_dir(app, graph_id)
        .and_then(|graph_dir| read_song_store(&graph_dir, graph_id))
        .map(|store| song_music_json(&store))
        .unwrap_or_else(|error| {
            serde_json::json!({
                "verses": [],
                "verse_count": 0,
                "verseCount": 0,
                "source": "local-song-store",
                "error": error,
            })
        })
}

pub(super) fn mcp_local_music(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    // Per-observer Song (fix #3): route by the wire-supplied observer; empty/absent
    // ⇒ the shared singleton (today's behavior, byte-identical store + graph).
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let store = read_song_store_for(&graph_dir, &graph_id, &observer)?;
    Ok(song_music_json(&store))
}

fn mcp_song_verse_index(arguments: &serde_json::Value) -> Option<i64> {
    ["verse_index", "verseIndex"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(serde_json::Value::as_i64))
}

pub(super) fn mcp_local_sing(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // Validate before reading or rotating authority: a misspelled field must
    // never become an empty verse that ejects an existing voice.
    let object = arguments.as_object().ok_or("sing arguments must be an object")?;
    for key in object.keys() {
        if !matches!(key.as_str(), "graph_id" | "graphId" | "verse" | "mode"
            | "verse_index" | "verseIndex" | "observer_agent_id" | "observerAgentId") {
            return Err(format!("sing does not accept '{key}'; supply Song text in 'verse'"));
        }
    }
    let verse = object.get("verse").and_then(serde_json::Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or("sing requires a non-empty string in 'verse'")?;
    let verse = unescape_literal_newlines(verse);
    let mode = match object.get("mode") {
        None => "verse",
        Some(value) => value.as_str().ok_or("sing mode must be a string")?,
    }.trim().to_ascii_lowercase();
    if !matches!(mode.as_str(), "verse" | "counterpoint" | "coda") {
        return Err(format!("mode must be 'verse', 'counterpoint', or 'coda' (got '{mode}')"));
    }
    let parsed = crate::geist_song_lines::parse_song_lines(&verse);
    let lines = parsed.len();
    if lines == 0 {
        return Err("sing requires at least one line of text in 'verse'; only newlines or / marks were sent. \
                    Nothing was changed."
            .to_string());
    }
    if lines > MAX_SONG_LINES {
        return Err(format!(
            "a {mode} has at most {MAX_SONG_LINES} lines; this one has {lines} (one line per newline; \
             a blank line between stanzas is not a line). Nothing was changed."
        ));
    }
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let _lease = crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(&app, &graph_id)?;
    let _guard = crate::legacy_geist_import::mutation_guard()?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    // Per-observer Song (fix #3): each witness sings into its OWN store + graph, so
    // co-tenants cannot wipe each other's Song. Empty/absent = the shared singleton.
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let mut store = read_song_store_for(&graph_dir, &graph_id, &observer)?;
    let now = timestamp();
    let mut result = serde_json::Map::new();

    match mode.as_str() {
        "verse" => {
            store.verses.insert(
                0,
                LocalSongVerse {
                    text: verse,
                    counterpoints: Vec::new(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                },
            );
            let mut ejected_count = 0usize;
            if store.verses.len() > MAX_SONG_VERSES {
                let ejected = store.verses.split_off(MAX_SONG_VERSES);
                ejected_count = ejected.len();
                store.archives.push(LocalSongArchiveRecord {
                    archived_at: now.clone(),
                    verses: ejected,
                });
                if let Some(coda) = &mut store.coda {
                    coda.ejections_remaining -= ejected_count as i64;
                    if coda.ejections_remaining <= 0 {
                        store.coda = None;
                    }
                }
            }
            result.insert(
                "verse_count".to_string(),
                serde_json::json!(store.verses.len()),
            );
            result.insert(
                "verseCount".to_string(),
                serde_json::json!(store.verses.len()),
            );
            if ejected_count > 0 {
                result.insert("ejected".to_string(), serde_json::json!(ejected_count));
                let archive_doc = crate::legacy_geist_import::projection_document_id(&graph_dir, &past_songs_doc_id(&observer))?;
                result.insert("archive_doc_id".to_string(), serde_json::json!(archive_doc));
                result.insert("archiveDocId".to_string(), serde_json::json!(archive_doc));
            }
        }
        "counterpoint" => {
            let verse_index = mcp_song_verse_index(arguments).ok_or_else(|| {
                "verse_index is required for counterpoint mode (0, -1, or -2)".to_string()
            })?;
            let index = if verse_index == 0 {
                0usize
            } else if verse_index < 0 {
                verse_index.unsigned_abs() as usize
            } else {
                return Err(format!(
                    "verse_index must be 0, -1, or -2 (got {verse_index})"
                ));
            };
            if index >= store.verses.len() {
                return Err(format!(
                    "Verse {verse_index} does not exist (Song has {} verses)",
                    store.verses.len()
                ));
            }
            let target = &mut store.verses[index];
            let total_voices = 1 + target.counterpoints.len();
            if total_voices >= MAX_SONG_VOICES {
                return Err(format!(
                    "Verse {verse_index} already has {total_voices} parts (max {MAX_SONG_VOICES}). Cannot add another counterpoint."
                ));
            }
            target.counterpoints.push(verse);
            target.updated_at = now.clone();
            let new_total = 1 + target.counterpoints.len();
            result.insert("verse_index".to_string(), serde_json::json!(verse_index));
            result.insert("verseIndex".to_string(), serde_json::json!(verse_index));
            result.insert("voice_number".to_string(), serde_json::json!(new_total));
            result.insert("voiceNumber".to_string(), serde_json::json!(new_total));
            result.insert("total_voices".to_string(), serde_json::json!(new_total));
            result.insert("totalVoices".to_string(), serde_json::json!(new_total));
        }
        "coda" => {
            store.coda = Some(LocalSongCoda {
                text: verse,
                ejections_remaining: CODA_EJECTION_LIFETIME,
                created_at: now.clone(),
            });
            result.insert("coda_set".to_string(), serde_json::json!(true));
            result.insert("codaSet".to_string(), serde_json::json!(true));
            result.insert(
                "ejections_remaining".to_string(),
                serde_json::json!(CODA_EJECTION_LIFETIME),
            );
            result.insert(
                "ejectionsRemaining".to_string(),
                serde_json::json!(CODA_EJECTION_LIFETIME),
            );
        }
        _ => unreachable!(),
    }

    let mut warnings = persist_song_store(&app, &graph_dir, &store)?;
    if let Err(error) = touch_graph_updated_at(&graph_dir) { warnings.push(error.to_string()); }
    if let Err(error) = touch_profile_updated_at(&app) { warnings.push(error); }
    result.insert("projection_warnings".into(), serde_json::json!(warnings));

    if let Some(coda) = &store.coda {
        if !result.contains_key("coda_set") {
            result.insert(
                "coda_ejections_remaining".to_string(),
                serde_json::json!(coda.ejections_remaining),
            );
            result.insert(
                "codaEjectionsRemaining".to_string(),
                serde_json::json!(coda.ejections_remaining),
            );
        }
    }
    // How the text was read, so a singer sees at once whether its lines and
    // stanzas landed as meant. Stanzas are separated by "" in `lines`.
    let mut read_as = Vec::new();
    for (index, (line, has_break)) in parsed.iter().enumerate() {
        read_as.push(line.clone());
        if *has_break && index + 1 < parsed.len() {
            read_as.push(String::new());
        }
    }
    let stanzas = 1 + parsed[..parsed.len().saturating_sub(1)]
        .iter()
        .filter(|(_, has_break)| *has_break)
        .count();
    result.insert("lineCount".to_string(), serde_json::json!(lines));
    result.insert("stanzaCount".to_string(), serde_json::json!(stanzas));
    result.insert("lines".to_string(), serde_json::json!(read_as));
    result.insert("mode".to_string(), serde_json::json!(mode));
    result.insert("source".to_string(), serde_json::json!("local-song-store"));
    Ok(serde_json::Value::Object(result))
}

/// Some clients send a verse with its newlines escaped, as the two characters
/// `\n`, which would make the whole verse one line. When the text has no real
/// newline but does have escaped ones, read them as newlines.
fn unescape_literal_newlines(text: &str) -> String {
    if text.contains('\n') || !text.contains("\\n") {
        return text.to_string();
    }
    text.replace("\\r\\n", "\n").replace("\\n", "\n")
}

#[cfg(test)]
mod sing_input_tests {
    use super::unescape_literal_newlines;

    #[test]
    fn escaped_newlines_are_read_only_when_there_are_no_real_ones() {
        assert_eq!(unescape_literal_newlines("one\\ntwo\\n\\nthree"), "one\ntwo\n\nthree");
        assert_eq!(unescape_literal_newlines("one\\r\\ntwo"), "one\ntwo");
        // A real newline means the text is already as meant.
        assert_eq!(unescape_literal_newlines("a \\n b\nc"), "a \\n b\nc");
        assert_eq!(unescape_literal_newlines("plain"), "plain");
    }

    #[test]
    fn text_of_only_marks_reads_as_no_lines() {
        // mcp_local_sing refuses these instead of storing an empty verse.
        for text in ["\\n", "\\n\\n", "/", " / \n/"] {
            let read = crate::geist_song_lines::parse_song_lines(&unescape_literal_newlines(text));
            assert!(read.is_empty(), "{text:?} read as {read:?}");
        }
    }
}
