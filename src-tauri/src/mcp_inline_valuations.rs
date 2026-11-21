//! MCP-only inline directives. Ordinary editor/import parsing remains literal.
use crate::app_runtime::AppHandle;
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

static MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)(\{!(\d)?(?:,([+-]?\d))?\})([\t ]*(?:\{#[a-zA-Z0-9_-]+(?::[a-zA-Z0-9_-]+)?\}[\t ]*)*)$").unwrap()
});

#[derive(Clone, Debug)]
pub(crate) struct Rating {
    index: usize,
    block_id: Option<String>,
    importance: Option<i64>,
    valence: Option<i64>,
}

pub(crate) struct Prepared {
    pub(crate) arguments: Value,
    pub(crate) ratings: Vec<Rating>,
    operation_id: String,
}

fn score(value: &Value, min: i64, max: i64) -> Option<i64> {
    let n = value.as_i64().or_else(|| value.as_str()?.parse().ok())?;
    (min..=max).contains(&n).then_some(n)
}

fn attributes(node: &mut Value, index: usize, ratings: &mut Vec<Rating>) {
    let Some(attrs) = node.get_mut("attrs").and_then(Value::as_object_mut) else {
        return;
    };
    let importance = attrs
        .get("data-val-importance")
        .and_then(|v| score(v, 0, 5));
    let valence = attrs.get("data-val-valence").and_then(|v| score(v, -5, 5));
    if importance.is_some() || valence.is_some() {
        if importance.is_some() {
            attrs.remove("data-val-importance");
        }
        if valence.is_some() {
            attrs.remove("data-val-valence");
        }
        ratings.push(Rating {
            index,
            block_id: None,
            importance,
            valence,
        });
    }
}

struct TextRun {
    path: Vec<usize>,
    start: usize,
    end: usize,
    eligible: bool,
}

fn collect_runs(
    node: &Value,
    path: &mut Vec<usize>,
    text: &mut String,
    runs: &mut Vec<TextRun>,
    eligible: bool,
) {
    if node["type"] == "hardBreak" {
        text.push('\n');
        return;
    }
    let eligible = eligible
        && node["type"] != "codeBlock"
        && node["type"] != "tableCell"
        && node["type"] != "tableHeader"
        && !node["marks"]
            .as_array()
            .is_some_and(|marks| marks.iter().any(|mark| mark["type"] == "code"));
    if let Some(value) = node["text"].as_str() {
        let start = text.len();
        text.push_str(value);
        runs.push(TextRun {
            path: path.clone(),
            start,
            end: text.len(),
            eligible,
        });
    }
    if let Some(children) = node["content"].as_array() {
        for (i, child) in children.iter().enumerate() {
            path.push(i);
            collect_runs(child, path, text, runs, eligible);
            path.pop();
        }
    }
    if matches!(
        node["type"].as_str(),
        Some("paragraph" | "heading" | "listItem" | "blockquote" | "codeBlock")
    ) {
        text.push('\n');
    }
}

fn extract_attributes(node: &mut Value, index: usize, ratings: &mut Vec<Rating>) {
    attributes(node, index, ratings);
    if let Some(children) = node.get_mut("content").and_then(Value::as_array_mut) {
        for child in children {
            extract_attributes(child, index, ratings);
        }
    }
}

fn extract(
    node: &mut Value,
    index: usize,
    ratings: &mut Vec<Rating>,
    fence: &mut Option<(char, usize)>,
    plain: bool,
) {
    extract_attributes(node, index, ratings);
    let mut text = String::new();
    let mut runs = Vec::new();
    collect_runs(node, &mut Vec::new(), &mut text, &mut runs, true);
    let mut removed = Vec::new();
    let mut offset = 0;
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        let first = trimmed.chars().next();
        let count = first
            .map(|c| trimmed.chars().take_while(|x| *x == c).count())
            .unwrap_or(0);
        let is_fence = plain && matches!(first, Some('`' | '~')) && count >= 3;
        if is_fence {
            match *fence {
                None => *fence = Some((first.unwrap(), count)),
                Some((c, n))
                    if Some(c) == first && count >= n && trimmed[count..].trim().is_empty() =>
                {
                    *fence = None
                }
                _ => {}
            }
        } else if fence.is_none() {
            if let Some(caps) = MARKER.captures(line) {
                let importance = caps.get(2).and_then(|m| score(&json!(m.as_str()), 0, 5));
                let valence = caps.get(3).and_then(|m| score(&json!(m.as_str()), -5, 5));
                let marker = caps.get(1).unwrap();
                let start = offset + marker.start();
                let end = offset + marker.end();
                let protected = runs
                    .iter()
                    .any(|run| !run.eligible && run.start < end && run.end > start);
                let valid = (caps.get(2).is_none() || importance.is_some())
                    && (caps.get(3).is_none() || valence.is_some());
                if !protected && valid && (importance.is_some() || valence.is_some()) {
                    removed.push((start, end));
                    ratings.push(Rating {
                        index,
                        block_id: None,
                        importance,
                        valence,
                    });
                }
            }
        }
        offset += line.len() + 1;
    }
    for run in runs {
        let mut target = &mut *node;
        for i in run.path {
            target = &mut target["content"][i];
        }
        let Some(original) = target["text"].as_str() else {
            continue;
        };
        let mut clean = original.to_owned();
        for &(start, end) in removed.iter().rev() {
            let start = start.max(run.start);
            let end = end.min(run.end);
            if start < end {
                clean.replace_range(start - run.start..end - run.start, "");
            }
        }
        target["text"] = json!(clean);
    }
}

fn operation_id(args: &Value) -> Result<String, String> {
    let Some(value) = args.get("operationId") else {
        return Ok(format!("op-{}", uuid::Uuid::new_v4().simple()));
    };
    let id = value.as_str().ok_or("operationId must be a string")?;
    if id.is_empty()
        || id.len() > 160
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b':'))
    {
        return Err("operationId must be <=160 characters of [A-Za-z0-9._:-]".into());
    }
    Ok(id.to_owned())
}

fn has_list_container(node: &Value) -> bool {
    matches!(
        node["type"].as_str(),
        Some("bulletList" | "orderedList" | "taskList")
    ) || node["content"]
        .as_array()
        .is_some_and(|nodes| nodes.iter().any(has_list_container))
}

fn plain_placeholders(
    content: &str,
    seed: &str,
) -> (String, Vec<(String, Option<i64>, Option<i64>)>) {
    let mut tokens = Vec::new();
    let mut fence = None;
    let mut lines = Vec::new();
    for line in content
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
    {
        let trimmed = line.trim_start();
        let first = trimmed.chars().next();
        let count = first
            .map(|c| trimmed.chars().take_while(|v| *v == c).count())
            .unwrap_or(0);
        if matches!(first, Some('`' | '~')) && count >= 3 {
            match fence {
                None => fence = Some((first.unwrap(), count)),
                Some((c, n))
                    if Some(c) == first && count >= n && trimmed[count..].trim().is_empty() =>
                {
                    fence = None
                }
                _ => {}
            }
            lines.push(line.to_string());
            continue;
        }
        let mut out = line.to_string();
        if fence.is_none() {
            if let Some(caps) = MARKER.captures(line) {
                let i = caps.get(2).and_then(|m| score(&json!(m.as_str()), 0, 5));
                let v = caps.get(3).and_then(|m| score(&json!(m.as_str()), -5, 5));
                if (caps.get(2).is_none() || i.is_some())
                    && (caps.get(3).is_none() || v.is_some())
                    && (i.is_some() || v.is_some())
                {
                    let marker = caps.get(1).unwrap();
                    let token = format!("\u{e000}INLINE-{seed}-{}\u{e001}", tokens.len());
                    out.replace_range(marker.start()..marker.end(), &token);
                    tokens.push((token, i, v));
                }
            }
        }
        lines.push(out);
    }
    (lines.join("\n"), tokens)
}

fn extract_plain(
    node: &mut Value,
    index: usize,
    tokens: &[(String, Option<i64>, Option<i64>)],
    ratings: &mut Vec<Rating>,
) {
    if let Some(text) = node["text"].as_str() {
        let mut clean = text.to_string();
        for (token, importance, valence) in tokens {
            if clean.contains(token) {
                clean = clean.replace(token, "");
                ratings.push(Rating {
                    index,
                    block_id: None,
                    importance: *importance,
                    valence: *valence,
                });
            }
        }
        node["text"] = json!(clean);
    }
    if let Some(children) = node.get_mut("content").and_then(Value::as_array_mut) {
        for child in children {
            extract_plain(child, index, tokens, ratings);
        }
    }
}

pub(crate) fn prepare(name: &str, args: &Value) -> Result<Prepared, String> {
    let mut prepared = Prepared {
        arguments: args.clone(),
        ratings: Vec::new(),
        operation_id: String::new(),
    };
    if !matches!(name, "write_document" | "insert_blocks" | "update_blocks") {
        return Ok(prepared);
    }
    prepared.operation_id = operation_id(args)?;
    if name == "write_document"
        && args.get("content").is_some()
        && (args.get("tiptapJson").is_some() || args.get("tiptap_json").is_some())
    {
        return Err("write_document accepts exactly one of content or tiptapJson".into());
    }
    if name == "update_blocks" {
        let key = if args.get("edits").is_some() {
            "edits"
        } else {
            "updates"
        };
        if let Some(edits) = prepared
            .arguments
            .get_mut(key)
            .and_then(Value::as_array_mut)
        {
            for (i, edit) in edits.iter_mut().enumerate() {
                extract_edit(edit, i, &mut prepared.ratings);
            }
        } else {
            extract_edit(&mut prepared.arguments, 0, &mut prepared.ratings);
        }
        if !prepared.ratings.is_empty() {
            if prepared.ratings.len() > 256 {
                return Err("At most 256 inline valuations per call".into());
            }
            prepared.arguments["operationId"] = json!(prepared.operation_id);
        }
        return Ok(prepared);
    }
    let from_content = args.get("content").and_then(Value::as_str);
    // Cheap bypass: keep unmarked writes on exactly their existing path.
    if let Some(content) = from_content {
        if !content.contains("{!") && !content.contains("data-val-") {
            return Ok(prepared);
        }
    }
    let mut parsed = if let Some(content) = from_content {
        crate::crdt_engine::content_parse::parse_write_content_for_operation(
            content,
            args["format"].as_str(),
            &prepared.operation_id,
        )?
    } else if let Some(tree) = args.get("tiptapJson").or_else(|| args.get("tiptap_json")) {
        crate::crdt_engine::content_parse::ParsedWriteContent {
            tiptap_json: tree.clone(),
            derived_title: None,
            source_format: "tiptap-json".into(),
            warnings: vec![],
        }
    } else if let Some(blocks) = args.get("blocks").and_then(Value::as_array) {
        let nodes = blocks
            .iter()
            .map(crate::crdt_engine::document_ops::block_to_tiptap_node)
            .collect::<Result<Vec<_>, _>>()?;
        crate::crdt_engine::content_parse::ParsedWriteContent {
            tiptap_json: json!({"type":"doc","content":nodes}),
            derived_title: None,
            source_format: "blocks".into(),
            warnings: vec![],
        }
    } else {
        return Ok(prepared);
    };
    let mut fence = None;
    let plain_tokens = if parsed.source_format == "plain" && from_content.is_some() {
        let (content, tokens) = plain_placeholders(from_content.unwrap(), &prepared.operation_id);
        if !tokens.is_empty() {
            parsed = crate::crdt_engine::content_parse::parse_write_content_for_operation(
                &content,
                Some("plain"),
                &prepared.operation_id,
            )?;
        }
        Some(tokens)
    } else {
        None
    };
    if let Some(nodes) = parsed
        .tiptap_json
        .get_mut("content")
        .and_then(Value::as_array_mut)
    {
        // Markdown image hoisting can leave a standalone rating paragraph.
        // Treat an adjacent marker-only paragraph as a directive for the image.
        if parsed.source_format == "markdown" {
            let mut i = 1;
            while i < nodes.len() {
                if nodes[i-1]["type"] == "image" && nodes[i]["type"] == "paragraph"
                    && nodes[i]["content"].as_array().is_some_and(|children|
                        children.iter().all(|child| child["type"] == "text" && child.get("marks").is_none())) {
                    let mut cleaned = nodes[i].clone();
                    let mut ratings = Vec::new();
                    extract(&mut cleaned, i-1, &mut ratings, &mut None, false);
                    let mut text = String::new();
                    collect_runs(&cleaned, &mut Vec::new(), &mut text, &mut Vec::new(), true);
                    if !ratings.is_empty() && text.trim().is_empty() {
                        prepared.ratings.extend(ratings);
                        nodes.remove(i);
                        continue;
                    }
                }
                i += 1;
            }
        }
        for (index, node) in nodes.iter_mut().enumerate() {
            if let Some(tokens) = &plain_tokens {
                extract_plain(node, index, tokens, &mut prepared.ratings);
                continue;
            }
            extract(
                node,
                index,
                &mut prepared.ratings,
                &mut fence,
                parsed.source_format == "plain",
            );
        }
    }
    if prepared.ratings.is_empty() {
        return Ok(prepared);
    }
    if has_list_container(&parsed.tiptap_json) {
        return Err("Inline valuations require flat listItem nodes; use Markdown/XML lists or flatten the structured TipTap list first".into());
    }
    if prepared.ratings.len() > 256 {
        return Err("At most 256 inline valuations per call".into());
    }
    if !parsed.warnings.is_empty() {
        return Err(format!(
            "Inline valuations require unambiguous content parsing: {}",
            parsed.warnings.join("; ")
        ));
    }
    prepared
        .arguments
        .as_object_mut()
        .unwrap()
        .remove("content");
    prepared
        .arguments
        .as_object_mut()
        .unwrap()
        .remove("tiptap_json");
    prepared.arguments.as_object_mut().unwrap().remove("blocks");
    for rating in &prepared.ratings {
        let node = &mut parsed.tiptap_json["content"][rating.index];
        if crate::crdt_engine::block_ops::read_block_id_from_json(node).is_none() {
            if node.get("attrs").is_none() {
                node["attrs"] = json!({});
            }
            if !node["attrs"].is_object() {
                return Err("Inline valuation target attrs must be an object".into());
            }
            node["attrs"]["data-block-id"] = json!(format!(
                "block-{:x}",
                Sha256::digest(format!("{}:{}", prepared.operation_id, rating.index).as_bytes())
            ));
        }
    }
    prepared.arguments["tiptapJson"] = parsed.tiptap_json;
    prepared.arguments["operationId"] = json!(prepared.operation_id);
    prepared.arguments["_inlineValuationWrite"] = json!(true);
    prepared.arguments["_inlineSourceFormat"] = json!(parsed.source_format);
    if name == "write_document" && from_content.is_some() {
        prepared.arguments["_inlineParsedContent"] = json!(true);
        if args.get("title").is_none() {
            if parsed.derived_title.is_some() {
                if let Some(heading) = prepared.arguments["tiptapJson"]["content"]
                    .as_array()
                    .and_then(|nodes| nodes.iter().find(|node| node["type"] == "heading"))
                {
                    prepared.arguments["title"] = json!(plain_text(heading));
                }
            }
        }
    }
    Ok(prepared)
}

fn plain_text(node: &Value) -> String {
    if let Some(text) = node["text"].as_str() {
        return text.into();
    }
    node["content"]
        .as_array()
        .map(|nodes| nodes.iter().map(plain_text).collect::<String>())
        .unwrap_or_default()
}

fn extract_edit(edit: &mut Value, index: usize, ratings: &mut Vec<Rating>) {
    let id = edit
        .get("blockId")
        .or_else(|| edit.get("block_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let key = if edit.get("attrs").is_some() {
        "attrs"
    } else {
        "attributes"
    };
    let mut node = json!({"attrs":edit.get(key).cloned().unwrap_or(Value::Null)});
    let start = ratings.len();
    attributes(&mut node, index, ratings);
    if ratings.len() > start {
        edit[key] = node["attrs"].clone();
        for rating in &mut ratings[start..] {
            rating.block_id = id.clone();
        }
    }
}

pub(crate) async fn apply(app: AppHandle, name: &str, prepared: &Prepared, result: &mut Value) {
    if prepared.ratings.is_empty() {
        return;
    }
    let ids = result
        .get("_inlineBlockIds")
        .or_else(|| result.get("block_ids"))
        .cloned();
    if let Some(object) = result.as_object_mut() {
        object.remove("_inlineBlockIds");
    }
    let graph = prepared
        .arguments
        .get("graphId")
        .or_else(|| prepared.arguments.get("graph_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let document = prepared
        .arguments
        .get("documentId")
        .or_else(|| prepared.arguments.get("document_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut errors = Vec::new();
    let mut requests = Vec::new();
    let mut events = Vec::new();
    let at_ms = crate::clock::epoch_millis() as i64;
    for (ordinal, rating) in prepared.ratings.iter().enumerate() {
        let id = rating
            .block_id
            .as_deref()
            .or_else(|| ids.as_ref()?.get(rating.index)?.as_str());
        let Some(id) = id else {
            errors.push(json!({"index":ordinal,"error":"final block ID unavailable"}));
            continue;
        };
        if name == "insert_blocks"
            && result["replayed"] != true
            && result["existing_block_ids"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|v| v.as_str() == Some(id)))
        {
            errors.push(json!({"index":ordinal,"blockId":id,"error":"existing block was not inserted; no rating applied"}));
            continue;
        }
        if name == "update_blocks"
            && !result["updated"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|v| v.as_str() == Some(id)))
        {
            errors.push(json!({"index":ordinal,"error":"target block was not updated"}));
            continue;
        }
        let event = format!(
            "inline-{:x}",
            Sha256::digest(
                format!("{graph}\0{document}\0{}\0{ordinal}", prepared.operation_id).as_bytes()
            )
        );
        let mut request = json!({"documentId":document,"blockId":id,"valuationEventId":event,"operationId":event,"atMs":at_ms});
        if let Some(n) = rating.importance {
            request["importance"] = json!(n);
        }
        if let Some(n) = rating.valence {
            request["valence"] = json!(n);
        }
        events.push(json!({"blockId":id,"valuationEventId":event,"importance":rating.importance,"valence":rating.valence,"atMs":at_ms}));
        requests.push(request);
    }
    let mut applied = 0;
    if !requests.is_empty() {
        match crate::source_sync::mcp_authoritative_value(
            app,
            &json!({"graphId":graph,"valuations":requests}),
        )
        .await
        {
            Ok(response) => {
                applied = response
                    .get("updatedCount")
                    .or_else(|| response.get("updated_count"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                if let Some(failures) = response["errors"].as_array() {
                    errors.extend(failures.clone());
                }
                if applied != requests.len() && errors.is_empty() {
                    errors.push(json!({"error":"valuation result did not confirm every entry"}));
                }
            }
            Err(error) => errors.push(json!({"error":error.to_string()})),
        }
    }
    result["valuations"] = json!({"requested":prepared.ratings.len(),"applied":applied,"errors":errors,"complete":errors.is_empty(),"operationId":prepared.operation_id,"events":events});
    if !errors.is_empty() {
        result["valuations"]["retry"] = json!({"graphId":graph,"valuations":requests});
        result["valuationWarning"]=json!("Content write succeeded; inline valuations are incomplete. Retry the returned valuation request with value(), not the content write. Event IDs prevent double-counting.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marked_nested_list_containers_fail_before_mutation() {
        let args = json!({"tiptapJson":{"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"Task {!4}"}]}]}]}]}});
        for tool in ["write_document", "insert_blocks"] {
            let error = prepare(tool, &args)
                .err()
                .expect("unsupported shape must fail closed");
            assert!(error.contains("flat listItem"));
        }
        let unmarked =
            json!({"tiptapJson":{"type":"doc","content":[{"type":"bulletList","content":[]}]}});
        assert_eq!(
            prepare("write_document", &unmarked).unwrap().arguments,
            unmarked
        );
    }
    #[test]
    fn values_cover_zero_negative_and_formatting_without_eating_examples() {
        let args = json!({"content":"# Heading {!0,-5}\n\n*Formatted {!3,+2}*\n\nInline `{!5}`\n\n```\nCode {!4}\n```\n\n**Midline {!4}** followed by prose.","format":"markdown"});
        let result = prepare("write_document", &args).unwrap();
        assert_eq!(result.ratings.len(), 2);
        assert_eq!(result.ratings[0].importance, Some(0));
        assert_eq!(result.ratings[0].valence, Some(-5));
        assert_eq!(result.arguments["title"], "Heading ");
        assert!(plain_text(&result.arguments["tiptapJson"]).contains("Code {!4}"));
        assert!(plain_text(&result.arguments["tiptapJson"]).contains("Midline {!4}"));
    }
    #[test]
    fn plain_fences_and_bad_ranges_remain_literal() {
        let args =
            json!({"content":"~~~\nExample {!4}\n~~~\nActual {!2}\nInvalid {!8}","format":"plain"});
        let result = prepare("insert_blocks", &args).unwrap();
        assert_eq!(result.ratings.len(), 1);
        assert!(plain_text(&result.arguments["tiptapJson"]).contains("Example {!4}"));
        assert!(plain_text(&result.arguments["tiptapJson"]).contains("Invalid {!8}"));
    }
    #[test]
    fn unmarked_calls_and_other_tools_are_identical() {
        let args = json!({"content":"normal prose","format":"markdown"});
        assert_eq!(prepare("write_document", &args).unwrap().arguments, args);
        let args = json!({"content":"literal {!5}"});
        assert_eq!(prepare("remember", &args).unwrap().arguments, args);
    }
}

#[cfg(test)]
mod markdown_review_tests {
    use super::*;
    #[test]
    fn image_directive_targets_image_not_empty_paragraph() {
        let prepared=prepare("write_document", &json!({"content":"![d](x.png) {!4}","format":"markdown"})).unwrap();
        assert_eq!(prepared.ratings.len(),1);
        assert_eq!(prepared.ratings[0].index,0);
        let nodes=prepared.arguments["tiptapJson"]["content"].as_array().unwrap();
        assert_eq!(nodes.len(),1,"{:?}",prepared.arguments);
        assert_eq!(nodes[0]["type"],"image");
    }
    #[test]
    fn table_marker_stays_literal_even_when_other_directives_apply() {
        let prepared=prepare("write_document", &json!({"content":"Paragraph {!4}\n\n| Syntax | Value |\n| --- | --- |\n| a | {!3} |", "format":"markdown"})).unwrap();
        assert_eq!(prepared.ratings.len(),1);
        assert_eq!(prepared.ratings[0].index,0);
        assert!(prepared.arguments["tiptapJson"].to_string().contains("{!3}"));
    }
    #[test]
    fn table_cell_marker_remains_literal_without_rating_table() {
        let prepared=prepare("write_document", &json!({"content":"| Syntax | Value |\n| --- | --- |\n| a | {!3} |", "format":"markdown"})).unwrap();
        assert!(prepared.ratings.is_empty());
        assert!(prepared.arguments["content"].as_str().unwrap().contains("{!3}"));
    }
}
