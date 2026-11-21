//! TipTap JSON → Markdown (GFM) serializer for `read_document(format=markdown)`
//! and the markdown document export.
//!
//! The previous export rendered the flattened block projection (plain text per
//! block), so every inline mark, link target, list nesting level, ordered
//! number, multi-line quote, table and image was lost (hosted Hoja QA
//! 2026-09-23). This walks the canonical TipTap document instead and is the
//! inverse of `content_parse::markdown_to_tiptap_json` for everything that
//! parser can represent: `markdown → store → markdown` is stable over the
//! round-trip corpus in `tests/markdown_round_trip.rs`.
//!
//! Garden's list model is FLAT: every `listItem` is a top-level block carrying
//! `listType` (bullet | ordered | task), `indent` and `checked`. Consecutive
//! items are emitted as one tight Markdown list, nesting by `indent`, with
//! ordered items numbered by their position among siblings.
//!
//! Intentionally lossy (no Markdown or schema equivalent): text colour /
//! highlight / font / comment and wire marks (the text is kept), block ids,
//! image `size`, table `colspan`/`rowspan`/`colwidth` (GFM has no spans),
//! a non-header first table row (GFM requires a header row, so it becomes
//! one), multiple blocks inside a table cell (joined with a space), and the
//! wikilink target (rendered as `[[label]]`). Unknown nodes degrade to their
//! text content.

use serde_json::Value;

/// Render a TipTap `doc` (or any node with `content`) as Markdown.
pub(crate) fn tiptap_json_to_markdown(doc: &Value) -> String {
    let mut footnotes: Vec<(String, String)> = Vec::new();
    let blocks = children(doc);
    let mut out = render_blocks(blocks, &mut footnotes);
    if !footnotes.is_empty() {
        let defs = footnotes
            .iter()
            .map(|(label, content)| format!("[^{label}]: {}", escape_text(content, false)))
            .collect::<Vec<_>>()
            .join("\n");
        if out.is_empty() {
            out = defs;
        } else {
            out.push_str("\n\n");
            out.push_str(&defs);
        }
    }
    out
}

fn children(node: &Value) -> &[Value] {
    node.get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn node_type(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or("")
}

fn attr<'a>(node: &'a Value, key: &str) -> Option<&'a Value> {
    node.get("attrs")
        .and_then(|attrs| attrs.get(key))
        .filter(|v| !v.is_null())
}

fn attr_str<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    attr(node, key).and_then(Value::as_str)
}

fn attr_num(node: &Value, key: &str) -> Option<f64> {
    match attr(node, key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

fn render_blocks(blocks: &[Value], footnotes: &mut Vec<(String, String)>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    while index < blocks.len() {
        if node_type(&blocks[index]) == "listItem" {
            // One Markdown list per run of items whose top-level (indent 0)
            // list type stays the same; a type change starts a new list.
            let start = index;
            let top_type =
                |item: &Value| attr_str(item, "listType").unwrap_or("bullet").to_string();
            let mut group_type: Option<String> = None;
            while index < blocks.len() && node_type(&blocks[index]) == "listItem" {
                let item = &blocks[index];
                if attr_num(item, "indent").unwrap_or(0.0) <= 0.0 || index == start {
                    let kind = top_type(item);
                    match &group_type {
                        Some(existing) if *existing != kind => break,
                        _ => group_type = Some(kind),
                    }
                }
                index += 1;
            }
            parts.push(render_flat_list(&blocks[start..index], footnotes));
            continue;
        }
        if let Some(rendered) = render_block(&blocks[index], footnotes) {
            parts.push(rendered);
        }
        index += 1;
    }
    parts.join("\n\n")
}

fn render_block(node: &Value, footnotes: &mut Vec<(String, String)>) -> Option<String> {
    let rendered = match node_type(node) {
        "paragraph" => {
            let text = render_inlines(children(node), footnotes, InlineContext::Block);
            if text.trim().is_empty() {
                return None;
            }
            escape_line_starts(&text)
        }
        "heading" => {
            let level = attr_num(node, "level").unwrap_or(1.0).clamp(1.0, 6.0) as usize;
            let text = render_inlines(children(node), footnotes, InlineContext::Heading);
            format!("{} {}", "#".repeat(level), text.trim())
        }
        "codeBlock" => {
            let language = attr_str(node, "language").unwrap_or("").trim().to_string();
            let code = plain_text(node);
            let fence = "`".repeat(longest_run(&code, '`').max(2) + 1);
            format!("{fence}{language}\n{code}\n{fence}")
        }
        "blockquote" => {
            let inner = render_blocks(children(node), footnotes);
            inner
                .lines()
                .map(|line| {
                    if line.is_empty() {
                        ">".to_string()
                    } else {
                        format!("> {line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "horizontalRule" => "---".to_string(),
        "image" => render_image(node, InlineContext::Block),
        "table" => render_table(node, footnotes),
        "mathBlock" => {
            let latex = attr_str(node, "latex")
                .map(str::to_string)
                .unwrap_or_else(|| plain_text(node));
            format!("$$\n{latex}\n$$")
        }
        "hardBreak" => return None,
        "doc" => render_blocks(children(node), footnotes),
        // Stray inline content at block level, or a block type Markdown has no
        // form for: keep its text.
        _ => {
            let text = if children(node).iter().any(|c| node_type(c) == "text") {
                render_inlines(children(node), footnotes, InlineContext::Block)
            } else {
                render_blocks(children(node), footnotes)
            };
            if text.trim().is_empty() {
                return None;
            }
            text
        }
    };
    Some(rendered)
}

/// Render a run of consecutive flat `listItem`s as one tight list.
fn render_flat_list(items: &[Value], footnotes: &mut Vec<(String, String)>) -> String {
    // Per nesting depth: the column where the item content starts (so a child
    // lands inside its parent) and the running ordered counter.
    let mut columns: Vec<usize> = Vec::new();
    let mut counters: Vec<u64> = Vec::new();
    let mut last_type: Vec<String> = Vec::new();
    let mut lines: Vec<String> = Vec::new();

    for item in items {
        let requested = attr_num(item, "indent").unwrap_or(0.0).max(0.0) as usize;
        // A child cannot be deeper than one level below the previous item.
        let depth = requested.min(columns.len());
        columns.truncate(depth);
        counters.truncate(depth + 1);
        last_type.truncate(depth + 1);

        let list_type = attr_str(item, "listType").unwrap_or("bullet").to_string();
        if counters.len() <= depth {
            counters.push(0);
            last_type.push(list_type.clone());
        } else if last_type[depth] != list_type {
            counters[depth] = 0;
            last_type[depth] = list_type.clone();
        }
        counters[depth] += 1;

        let base = columns.last().copied().unwrap_or(0);
        let marker = match list_type.as_str() {
            "ordered" => format!("{}. ", counters[depth]),
            "task" => {
                let checked = attr(item, "checked").map(truthy).unwrap_or(false);
                format!("- [{}] ", if checked { "x" } else { " " })
            }
            _ => "- ".to_string(),
        };
        // Task items nest under the content after "- ", not after the box.
        let content_column = base
            + if list_type == "task" {
                2
            } else {
                marker.chars().count()
            };
        columns.push(content_column);

        let blocks = children(item);
        let (first, rest) = match blocks.first() {
            Some(first) if node_type(first) == "paragraph" => (
                render_inlines(children(first), footnotes, InlineContext::Block),
                &blocks[1..],
            ),
            _ => (String::new(), blocks),
        };
        let pad = " ".repeat(base);
        let first = escape_line_starts(&first);
        let mut first_lines = first.lines();
        lines.push(format!("{pad}{marker}{}", first_lines.next().unwrap_or("")));
        let cont = " ".repeat(content_column);
        for line in first_lines {
            lines.push(format!("{cont}{line}"));
        }
        for block in rest {
            if let Some(rendered) = render_block(block, footnotes) {
                lines.push(String::new());
                for line in rendered.lines() {
                    lines.push(if line.is_empty() {
                        String::new()
                    } else {
                        format!("{cont}{line}")
                    });
                }
            }
        }
    }
    lines.join("\n")
}

fn table_escape(text: &str, context: InlineContext) -> String {
    if context == InlineContext::TableCell { text.replace('|', "\\|") } else { text.to_string() }
}

fn render_image(node: &Value, context: InlineContext) -> String {
    let src = attr_str(node, "src").unwrap_or("");
    let alt = attr_str(node, "alt").unwrap_or("");
    let title = attr_str(node, "title").filter(|t| !t.is_empty());
    let alt = alt
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]");
    let src = if src.is_empty() || src.contains([' ', '(', ')', '<', '>']) {
        format!("<{}>", src.replace('<', "%3C").replace('>', "%3E"))
    } else {
        src.to_string()
    };
    let rendered = match title {
        Some(title) => format!("![{alt}]({src} \"{}\")", title.replace('"', "\\\"")),
        None => format!("![{alt}]({src})"),
    };
    table_escape(&rendered, context)
}

fn render_table(node: &Value, footnotes: &mut Vec<(String, String)>) -> String {
    let rows: Vec<&Value> = children(node)
        .iter()
        .filter(|r| node_type(r) == "tableRow")
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    let width = rows
        .iter()
        .map(|r| children(r).len())
        .max()
        .unwrap_or(0)
        .max(1);
    let render_row = |row: &Value, footnotes: &mut Vec<(String, String)>| -> Vec<String> {
        let mut cells: Vec<String> = children(row)
            .iter()
            .map(|cell| render_table_cell(cell, footnotes))
            .collect();
        cells.resize(width, String::new());
        cells
    };
    let alignments: Vec<Option<String>> = (0..width)
        .map(|column| {
            children(rows[0])
                .get(column)
                .and_then(|cell| children(cell).first())
                .and_then(|p| attr_str(p, "textAlign"))
                .map(str::to_string)
        })
        .collect();
    let row_line = |cells: &[String]| -> String {
        let mut line = String::from("|");
        for cell in cells {
            if cell.is_empty() {
                line.push_str(" |");
            } else {
                line.push_str(&format!(" {cell} |"));
            }
        }
        line
    };
    let mut lines = Vec::new();
    let header = render_row(rows[0], footnotes);
    lines.push(row_line(&header));
    let delimiter = alignments
        .iter()
        .map(|align| match align.as_deref() {
            Some("left") => ":---",
            Some("center") => ":---:",
            Some("right") => "---:",
            _ => "---",
        })
        .collect::<Vec<_>>()
        .join(" | ");
    lines.push(format!("| {delimiter} |"));
    for row in &rows[1..] {
        let cells = render_row(row, footnotes);
        lines.push(row_line(&cells));
    }
    lines.join("\n")
}

fn render_table_cell(cell: &Value, footnotes: &mut Vec<(String, String)>) -> String {
    children(cell)
        .iter()
        .filter_map(|block| match node_type(block) {
            "paragraph" | "heading" => {
                let text = render_inlines(children(block), footnotes, InlineContext::TableCell);
                (!text.trim().is_empty()).then(|| text.trim().to_string())
            }
            "image" => Some(render_image(block, InlineContext::TableCell)),
            _ => {
                let text = plain_text(block).replace('|', "\\|").replace('\n', " ");
                (!text.trim().is_empty()).then(|| text.trim().to_string())
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Inlines
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum InlineContext {
    Block,
    Heading,
    TableCell,
}

/// Markdown-representable marks, in canonical nesting order (outermost
/// first). `code` is innermost because nothing can nest inside a code span.
const MARK_ORDER: &[&str] = &["link", "bold", "italic", "strike", "code"];

#[derive(Clone, PartialEq)]
struct OpenMark {
    kind: &'static str,
    href: String,
    title: String,
}

fn marks_of(node: &Value) -> Vec<OpenMark> {
    let marks = node.get("marks").and_then(Value::as_array);
    let mut out = Vec::new();
    for kind in MARK_ORDER {
        let Some(mark) = marks.and_then(|m| m.iter().find(|m| node_type(m) == *kind)) else {
            continue;
        };
        out.push(OpenMark {
            kind,
            href: attr_str(mark, "href").unwrap_or("").to_string(),
            title: attr_str(mark, "title").unwrap_or("").to_string(),
        });
    }
    out
}

fn open_delim(mark: &OpenMark) -> String {
    match mark.kind {
        "link" => "[".to_string(),
        "bold" => "**".to_string(),
        "italic" => "*".to_string(),
        "strike" => "~~".to_string(),
        _ => String::new(),
    }
}

fn close_delim(mark: &OpenMark, context: InlineContext) -> String {
    match mark.kind {
        "link" => {
            let href = if mark.href.contains([' ', '(', ')', '<', '>']) {
                format!("<{}>", mark.href.replace('<', "%3C").replace('>', "%3E"))
            } else {
                mark.href.clone()
            };
            let rendered = if mark.title.is_empty() {
                format!("]({href})")
            } else {
                format!("]({href} \"{}\")", mark.title.replace('"', "\\\""))
            };
            table_escape(&rendered, context)
        }
        other => open_delim(&OpenMark {
            kind: other_static(other),
            href: String::new(),
            title: String::new(),
        }),
    }
}

fn other_static(kind: &str) -> &'static str {
    MARK_ORDER
        .iter()
        .find(|k| **k == kind)
        .copied()
        .unwrap_or("")
}

fn render_inlines(
    nodes: &[Value],
    footnotes: &mut Vec<(String, String)>,
    context: InlineContext,
) -> String {
    let mut out = String::new();
    let mut stack: Vec<OpenMark> = Vec::new();
    // Whitespace that trailed the previous text node; emitted after any
    // closing delimiters so `**bold **` never happens.
    let mut pending_ws = String::new();

    let close_to = |stack: &mut Vec<OpenMark>, keep: usize, out: &mut String| {
        while stack.len() > keep {
            let mark = stack.pop().expect("non-empty");
            out.push_str(&close_delim(&mark, context));
        }
    };

    for node in nodes {
        let kind = node_type(node);
        if kind != "text" {
            close_to(&mut stack, 0, &mut out);
            out.push_str(&std::mem::take(&mut pending_ws));
            out.push_str(&render_inline_atom(node, footnotes, context));
            continue;
        }
        let text = node.get("text").and_then(Value::as_str).unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let mut marks = marks_of(node);
        // Keep marks that are already open first (in stack order) so a mark
        // that spans several text nodes is opened once: `*a **b** c*`, not
        // `*a* ***b*** *c*`. `code` always stays innermost.
        marks.sort_by_key(|mark| {
            let open = stack.iter().position(|m| m == mark);
            (mark.kind == "code", open.is_none(), open.unwrap_or(0))
        });
        let is_code = marks.iter().any(|m| m.kind == "code");
        let (lead, core, trail) = if is_code {
            ("", text, "")
        } else {
            split_ws(text)
        };
        if core.is_empty() {
            // Whitespace-only run: plain text, marks unchanged.
            pending_ws.push_str(lead);
            continue;
        }
        let common = stack.iter().zip(&marks).take_while(|(a, b)| a == b).count();
        close_to(&mut stack, common, &mut out);
        out.push_str(&std::mem::take(&mut pending_ws));
        out.push_str(lead);
        for mark in &marks[common..] {
            out.push_str(&open_delim(mark));
            stack.push(mark.clone());
        }
        if is_code {
            out.push_str(&code_span(&table_escape(core, context)));
        } else {
            let escaped = escape_text(core, context == InlineContext::TableCell);
            if context == InlineContext::Heading { out.push_str(&escaped.replace('#', "\\#")); }
            else { out.push_str(&escaped); }
        }
        pending_ws.push_str(trail);
        // A code span is a closed unit: never leave `code` open across nodes.
        if is_code {
            if let Some(pos) = stack.iter().position(|m| m.kind == "code") {
                close_to(&mut stack, pos, &mut out);
            }
        }
    }
    close_to(&mut stack, 0, &mut out);
    out.push_str(&pending_ws);
    if context == InlineContext::TableCell {
        out = out.replace('\n', " ");
    }
    out
}

fn render_inline_atom(
    node: &Value,
    footnotes: &mut Vec<(String, String)>,
    context: InlineContext,
) -> String {
    match node_type(node) {
        "hardBreak" => match context {
            InlineContext::Block => "\\\n".to_string(),
            _ => " ".to_string(),
        },
        "footnote" => {
            let label = attr_str(node, "label").unwrap_or("").to_string();
            let content = attr_str(node, "content").unwrap_or("").to_string();
            let label = if label.is_empty() {
                (footnotes.len() + 1).to_string()
            } else {
                label
            };
            if !footnotes.iter().any(|(l, _)| *l == label) {
                footnotes.push((label.clone(), content));
            }
            format!("[^{label}]")
        }
        "wikilink" => match (attr_str(node, "label"), attr_str(node, "filePath")) {
            (Some(label), _) if !label.trim().is_empty() => format!("[[{label}]]"),
            // A file link with no label reads as its address.
            (_, Some(path)) => match attr_str(node, "region") {
                Some(region) if !region.is_empty() => format!("[[{path}#{region}]]"),
                _ => format!("[[{path}]]"),
            },
            _ => "[[Untitled]]".to_string(),
        },
        "tagChip" => format!("#{}", attr_str(node, "name").unwrap_or("")),
        "mathInline" => {
            let latex = attr_str(node, "latex")
                .map(str::to_string)
                .unwrap_or_else(|| plain_text(node));
            format!("${latex}$")
        }
        "image" => render_image(node, context),
        _ => escape_text(&plain_text(node), context == InlineContext::TableCell),
    }
}

fn split_ws(text: &str) -> (&str, &str, &str) {
    let trimmed_start = text.trim_start();
    let lead = &text[..text.len() - trimmed_start.len()];
    let core = trimmed_start.trim_end();
    let trail = &trimmed_start[core.len()..];
    (lead, core, trail)
}

fn code_span(text: &str) -> String {
    let ticks = "`".repeat(longest_run(text, '`') + 1);
    let pad = text.starts_with('`')
        || text.ends_with('`')
        || (text.starts_with(' ') && text.ends_with(' ') && !text.trim().is_empty());
    if pad {
        format!("{ticks} {text} {ticks}")
    } else {
        format!("{ticks}{text}{ticks}")
    }
}

fn longest_run(text: &str, ch: char) -> usize {
    let mut best = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == ch {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

/// Backslash-escape the characters that would otherwise start Markdown
/// syntax inside running text, while leaving ordinary prose (snake_case,
/// `[[wikilinks]]`, URLs) readable.
fn escape_text(text: &str, in_table: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        let next = chars.get(i + 1).copied();
        let escape = match c {
            '\\' => next.is_some_and(|n| n.is_ascii_punctuation()) || next.is_none(),
            '*' | '`' | '&' => true,
            '_' => {
                // Intraword underscores never delimit emphasis in CommonMark.
                !(prev.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric))
            }
            '~' => next == Some('~') || prev == Some('~'),
            // `[text](url)` / `[text]: url` need `](`, `]:` or `][`.
            ']' => matches!(next, Some('(') | Some(':') | Some('[')),
            '[' => next == Some('^'),
            '<' => next.is_some_and(|n| n.is_ascii_alphabetic() || matches!(n, '/' | '!' | '?')),
            '|' => in_table,
            _ => false,
        };
        if escape {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Escape a leading character on each line that Markdown would read as a
/// block marker (heading, quote, list, rule, fence, table-less setext).
fn escape_line_starts(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let trimmed = line.trim_start();
            let indent = &line[..line.len() - trimmed.len()];
            if indent.contains('\t') || indent.len() >= 4 {
                let first = if indent.starts_with('\t') { "&#9;" } else { "&#32;" };
                return format!("{first}{}", &line[1..]);
            }
            let needs = trimmed.starts_with('#')
                || trimmed.starts_with('>')
                || trimmed.starts_with("- ")
                || trimmed.starts_with("+ ")
                || trimmed == "-"
                || trimmed == "+"
                || trimmed.starts_with("---")
                || trimmed.starts_with("===")
                || (!trimmed.is_empty() && trimmed.chars().all(|c| c == '-' || c == '='))
                || trimmed.starts_with("~~~")
                || ["[ ] ", "[x] ", "[X] "].iter().any(|prefix| trimmed.starts_with(prefix))
                || ordered_marker_len(trimmed).is_some();
            if !needs {
                return line.to_string();
            }
            if let Some(digits) = ordered_marker_len(trimmed) {
                // "12. x" → "12\. x"
                return format!("{indent}{}\\{}", &trimmed[..digits], &trimmed[digits..]);
            }
            format!("{indent}\\{trimmed}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Length of the digit run when the line starts like an ordered-list marker.
fn ordered_marker_len(line: &str) -> Option<usize> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let rest = &line[digits..];
    let marker = rest.starts_with(". ") || rest.starts_with(") ") || rest == "." || rest == ")";
    marker.then_some(digits)
}

fn plain_text(node: &Value) -> String {
    if node_type(node) == "text" {
        return node
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
    }
    if node_type(node) == "hardBreak" {
        return "\n".to_string();
    }
    children(node).iter().map(plain_text).collect()
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty() && s != "false",
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::Null => false,
        _ => true,
    }
}
