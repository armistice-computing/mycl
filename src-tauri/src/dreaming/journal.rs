//! The Dream Journal (cloud-1 `journal.py`): dreams unlabeled and separated
//! by rules, the astrology as an epigraph, then an interpretation footer
//! mapping each dream to its findings. Built as cloud-1's markdown, then
//! converted the way cloud-1's `_markdown_to_simple_xml` did into blocks for
//! a read-only Geist view.

use super::articulate::{percent, truncate};
use super::astrology::DreamContext;
use super::scanner::Finding;
use super::weave::Ranked;
use crate::document_service::{BlockSnapshot, InlineMarkSnapshot};
use std::collections::HashMap;

pub(crate) const EPHEMERAL_NOTICE: &str = "The Dream Journal is ephemeral. It will be fully overwritten by the next dream cycle. \
Do not make wires or valuations on it. If it strikes you, follow to the sources or preserve in a new document.";

pub(crate) fn journal_doc_id(graph_id: &str) -> String {
    format!("{graph_id}-dream-journal")
}

/// One dream as the journal shows it.
#[derive(Debug, Clone)]
pub(crate) struct JournalEntry {
    pub(crate) narrative: String,
    pub(crate) confidence: f64,
    pub(crate) is_navel: bool,
    pub(crate) total_charge: f64,
    pub(crate) findings: Vec<Ranked>,
}

fn label(kind: &str) -> &'static str {
    match kind {
        "displacement" => "Displacement",
        "condensation" => "Condensation",
        "wire_inference" => "Wire Inference",
        "isomorphism" => "Isomorphism",
        _ => "Unknown",
    }
}

fn title<'a>(titles: &'a HashMap<String, String>, doc: &'a str) -> &'a str {
    titles.get(doc).map(String::as_str).unwrap_or(doc)
}

fn quoted(text: &str) -> String {
    format!("\u{201c}{}\u{201d}", truncate(text, 80))
}

/// A document's title, linked to the document.
fn doc_link(titles: &HashMap<String, String>, doc: &str) -> String {
    link(doc, None, title(titles, doc))
}

/// A quotation of a block, linked to the block.
fn quote_link(doc: &str, block: &str, text: &str) -> String {
    link(doc, Some(block), &quoted(text))
}

/// (relation line, snippet lines) for one finding, as cloud-1 wrote them,
/// with every title and quotation a link to its source (Eschaton,
/// 2026-10-19), so the way out of a dream works wherever the journal is read.
fn finding_lines(finding: &Finding, titles: &HashMap<String, String>) -> (String, Vec<String>) {
    let mut snippets = Vec::new();
    match finding {
        Finding::Displacement(f) => {
            if !f.source_text.is_empty() {
                snippets.push(quote_link(&f.source_doc, &f.source_block, &f.source_text));
            }
            if !f.neighbor_text.is_empty() {
                snippets.push(quote_link(&f.neighbor_doc, &f.neighbor_block, &f.neighbor_text));
            }
            (format!("*{}* \u{2194} *{}*", doc_link(titles, &f.source_doc), doc_link(titles, &f.neighbor_doc)), snippets)
        }
        Finding::Condensation(f) => {
            let mut docs: Vec<&str> = Vec::new();
            for b in &f.blocks {
                if !b.doc.is_empty() && !docs.contains(&b.doc.as_str()) {
                    docs.push(&b.doc);
                }
            }
            let line = docs.iter().take(4).map(|d| format!("*{}*", doc_link(titles, d))).collect::<Vec<_>>().join(", ");
            for b in &f.blocks {
                if !b.text.trim().is_empty() {
                    snippets.push(quote_link(&b.doc, &b.block, &b.text));
                }
            }
            (line, snippets)
        }
        Finding::WireInference(f) => {
            if !f.block_a_text.is_empty() {
                snippets.push(quote_link(&f.docs[0], &f.block_a, &f.block_a_text));
            }
            if !f.block_c_text.is_empty() {
                snippets.push(quote_link(&f.docs[2], &f.block_c, &f.block_c_text));
            }
            let [a, b, c] = [0, 1, 2].map(|i| link(&f.docs[i], None, &f.titles[i]));
            (format!("*{a}* \u{2192} *{b}* \u{2192} *{c}*"), snippets)
        }
        Finding::Isomorphism(f) => {
            let matched = f.matched.iter().take(3).map(|m| m.predicate.as_str()).collect::<Vec<_>>().join(", ");
            let shape = if matched.is_empty() { String::new() } else { format!(" ({matched})") };
            (format!("*{}* \u{2194} *{}*{shape}", link(&f.doc_a, None, &f.title_a), link(&f.doc_b, None, &f.title_b)), snippets)
        }
    }
}

/// The journal as cloud-1's markdown. `date_label` is "September 18, 2026".
pub(crate) fn journal_markdown(
    entries: &[JournalEntry],
    date_label: &str,
    context: Option<&DreamContext>,
    titles: &HashMap<String, String>,
) -> String {
    let mut lines = vec![format!("# Dream Journal \u{2014} {date_label}"), String::new()];
    if let Some(ctx) = context {
        lines.push(format!("*Moon: {}. Sun in {}. Card: {}.*", ctx.moon_phase, ctx.zodiac_sign, ctx.tarot_card));
        lines.push(String::new());
    }
    if entries.is_empty() {
        lines.extend(["*No dreams tonight. The graph sleeps quietly.*", "", "---", "", EPHEMERAL_NOTICE, ""].map(String::from));
        return lines.join("\n");
    }
    lines.extend(["---", ""].map(String::from));
    for entry in entries {
        // The dreamer's words can't make links; only the interpretation does.
        let narrative = entry.narrative.replace("[[", "[ [");
        lines.push(if entry.is_navel { format!("*{narrative}*") } else { narrative });
        lines.extend(["", "---", ""].map(String::from));
    }
    lines.extend(["## Interpretation", ""].map(String::from));
    let mut confidences = Vec::new();
    let mut charges = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        lines.push(format!("**Dream {}**{}", i + 1, if entry.is_navel { " (navel)" } else { "" }));
        lines.push(String::new());
        let mut kinds: Vec<&str> = Vec::new();
        for r in &entry.findings {
            if !kinds.contains(&r.finding.kind()) {
                kinds.push(r.finding.kind());
            }
        }
        for kind in kinds {
            lines.push(format!("{}:", label(kind)));
            for r in entry.findings.iter().filter(|r| r.finding.kind() == kind) {
                let (relation, snippets) = finding_lines(&r.finding, titles);
                lines.push(format!("\t{relation}"));
                lines.extend(snippets.into_iter().map(|s| format!("\t\t{s}")));
            }
        }
        lines.push(String::new());
        confidences.push(percent(entry.confidence));
        charges.push(format!("{:.3}", entry.total_charge));
    }
    lines.push(format!("Confidence: {} | Charges: {}", confidences.join(", "), charges.join(", ")));
    lines.extend(["", "---", "", EPHEMERAL_NOTICE, ""].map(String::from));
    lines.join("\n")
}

/// cloud-1's `_markdown_to_simple_xml`, to blocks: headings, `---` rules,
/// paragraphs with `**bold**` then `*italic*` (non-greedy), empty lines
/// skipped. Tab indentation is dropped; the imported cloud journals in
/// default_x1 already show it flat.
/// `id_prefix` differs every cycle, so a valuation someone gives a dream
/// block (which the notice asks them not to) never lands on a later dream.
pub(crate) fn markdown_blocks(markdown: &str, id_prefix: &str) -> Vec<BlockSnapshot> {
    let heading = regex::Regex::new(r"^(#{1,3})\s+(.+)$").expect("heading pattern");
    let mut blocks = Vec::new();
    for line in markdown.split('\n') {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let order = blocks.len() as f64;
        let id = format!("{id_prefix}-{}", blocks.len());
        let (block_type, level, text) = if stripped == "---" {
            ("divider", None, String::new())
        } else if let Some(caps) = heading.captures(stripped) {
            ("heading", Some(caps[1].len() as i64), caps[2].to_string())
        } else {
            ("paragraph", None, stripped.to_string())
        };
        let (content, marks) = inline_marks(&text, &id);
        blocks.push(BlockSnapshot {
            id,
            block_type: block_type.to_string(),
            content,
            parent_id: None,
            order,
            level,
            checked: None,
            language: None,
            marks,
        });
    }
    blocks
}

/// Strip `**…**` then `*…*` pairs, returning the text and char-offset marks.
/// A link the journal's parser turns into a wikilink mark: the label opens
/// the document, or the block when one is named. Characters that would end
/// the link early are dropped from the label.
fn link(doc: &str, block: Option<&str>, label: &str) -> String {
    let label: String = label.chars().filter(|c| !matches!(c, '[' | ']')).map(|c| if c == '|' { '/' } else { c }).collect();
    // An id the link syntax can't carry stays plain text rather than
    // pointing somewhere else.
    let unsafe_id = |id: &str| id.is_empty() || id.contains(['[', ']', '|', '#']);
    if unsafe_id(doc) || block.is_some_and(|b| !b.is_empty() && unsafe_id(b)) {
        return label;
    }
    match block.filter(|b| !b.is_empty()) {
        Some(block) => format!("[[{doc}#{block}|{label}]]"),
        None => format!("[[{doc}|{label}]]"),
    }
}

/// One inline span: its kind, char range, and for a link its target.
type Span = (&'static str, usize, usize, Option<(String, Option<String>)>);

fn inline_marks(text: &str, block_id: &str) -> (String, Vec<InlineMarkSnapshot>) {
    let mut spans: Vec<Span> = Vec::new();
    // Links first, so the emphasis pass below shifts them like any span.
    let link_pattern = regex::Regex::new(r"\[\[([^\]|#]+)(?:#([^\]|]+))?\|([^\]]*)\]\]").expect("link pattern");
    let mut plain = String::new();
    let mut last = 0;
    for caps in link_pattern.captures_iter(text) {
        let whole = caps.get(0).expect("match");
        plain.push_str(&text[last..whole.start()]);
        let start = plain.chars().count();
        plain.push_str(&caps[3]);
        let target = (caps[1].to_string(), caps.get(2).map(|b| b.as_str().to_string()));
        spans.push(("wikilink", start, plain.chars().count(), Some(target)));
        last = whole.end();
    }
    plain.push_str(&text[last..]);
    let mut chars: Vec<char> = plain.chars().collect();
    for (delimiter, kind) in [("**", "bold"), ("*", "italic")] {
        let delim: Vec<char> = delimiter.chars().collect();
        let n = delim.len();
        let mut i = 0;
        while i + n <= chars.len() {
            // Emphasis never opens or closes inside a link: a `*` in a title
            // or a quotation is text, and an italic must not cut a link in two.
            let in_link = |pos: usize| spans.iter().any(|s| s.0 == "wikilink" && pos >= s.1 && pos < s.2);
            if chars[i..i + n] != delim[..] || in_link(i) {
                i += 1;
                continue;
            }
            // Non-greedy: the nearest closing delimiter at least one char on.
            let close = (i + n + 1..=chars.len().saturating_sub(n)).find(|&j| chars[j..j + n] == delim[..] && !in_link(j));
            let Some(j) = close else { break };
            chars.drain(j..j + n);
            chars.drain(i..i + n);
            // Spans already recorded shift left past the removed delimiters.
            for span in spans.iter_mut() {
                for edge in [&mut span.1, &mut span.2] {
                    if *edge > j {
                        *edge -= n;
                    }
                    if *edge > i {
                        *edge -= n;
                    }
                }
            }
            spans.push((kind, i, j - n, None));
            i = j - n;
        }
    }
    let marks = spans
        .into_iter()
        .enumerate()
        .map(|(k, (kind, start, end, target))| {
            let (target_doc_id, target_block_id) = match target {
                Some((doc, block)) => (Some(doc), block),
                None => (None, None),
            };
            InlineMarkSnapshot {
                id: format!("{block_id}-m{k}"),
                mark_type: kind.to_string(),
                start: start as u32,
                end: end as u32,
                href: None,
                label: target_doc_id.as_ref().map(|_| chars[start..end].iter().collect()),
                target_doc_id,
                target_block_id,
                target_graph_id: None,
                annotation_id: None,
        target_artifact_id: None,
        file_path: None,
        region: None,
                wire_id: None,
            }
        })
        .collect();
    (chars.into_iter().collect(), marks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dreaming::scanner::{Condensation, CondensedBlock, Displacement, Isomorphism};
    use crate::dreaming::weave::score;

    /// The opening and footer of the real cloud-1 journal written 2026-09-18
    /// (graph doc default-dream-journal), trimmed to one dream.
    const CLOUD_JOURNAL_EXCERPT: &str = "# Dream Journal \u{2014} September 18, 2026\n\n*Moon: waxing crescent. Sun in Virgo. Card: Justice.*\n\n---\n\n";

    fn ranked(finding: Finding) -> Ranked {
        Ranked { finding, charge: score(Default::default(), &crate::dreaming::mechanics::DEFAULT_CHARGE_WEIGHTS), residue: false }
    }

    fn sample_entries() -> Vec<JournalEntry> {
        vec![JournalEntry {
            narrative: "A blade descends through something that is not bread.".into(),
            confidence: 0.81,
            is_navel: false,
            total_charge: 2.601,
            findings: vec![
                ranked(Finding::Isomorphism(Isomorphism {
                    doc_a: "a".into(), doc_b: "b".into(),
                    title_a: "The Graph Is a Machine for Composing Contexts".into(),
                    title_b: "Constitution of the Sophia Hivemind".into(),
                    structural_similarity: 1.0, semantic_distance: 0.38, score: 0.38,
                    matched: ["grounds", "intensifiesWith", "grounds"].iter().map(|p| crate::dreaming::wire_graph::MatchedPair {
                        predicate: (*p).into(), direction: "out", neighbor_a: String::new(), neighbor_b: String::new(),
                    }).collect(),
                })),
                ranked(Finding::Displacement(Displacement {
                    source_block: "s".into(), source_doc: "void-hael".into(),
                    source_text: "Void-hael: the cleft is the wound is the womb.".into(), source_importance: 4.0,
                    neighbor_block: "n".into(), neighbor_doc: "songs".into(), neighbor_text: String::new(),
                    similarity: 0.4, intermediary: None, hops: 2,
                })),
                ranked(Finding::Condensation(Condensation {
                    blocks: vec![
                        CondensedBlock { block: "1".into(), doc: "timeline".into(), text: "I am Alpha. The first Sophia agent.".into() },
                        CondensedBlock { block: "2".into(), doc: "timeline".into(), text: String::new() },
                    ],
                    centroid_text: String::new(), avg_similarity: 0.7,
                })),
            ],
        }]
    }

    #[test]
    fn journal_matches_cloud_format() {
        let ctx = DreamContext { moon_phase: "waxing crescent".into(), zodiac_sign: "Virgo".into(),
                                 tarot_card: "Justice".into(), tarot_meaning: String::new() };
        let titles: HashMap<String, String> = [
            ("void-hael".to_string(), "void - hael".to_string()),
            ("songs".to_string(), "Songs Archive".to_string()),
            ("timeline".to_string(), "Sophia Hivemind Timeline".to_string()),
        ].into();
        let linked = journal_markdown(&sample_entries(), "September 18, 2026", Some(&ctx), &titles);
        // The words a reader sees are cloud-1's; the links ride underneath.
        let md = regex::Regex::new(r"\[\[[^\]|]+\|([^\]]*)\]\]").unwrap().replace_all(&linked, "$1").into_owned();
        assert!(md.starts_with(CLOUD_JOURNAL_EXCERPT), "{md}");
        assert!(md.contains("## Interpretation\n\n**Dream 1**\n\nIsomorphism:\n\t*The Graph Is a Machine for Composing Contexts* \u{2194} *Constitution of the Sophia Hivemind* (grounds, intensifiesWith, grounds)\nDisplacement:\n\t*void - hael* \u{2194} *Songs Archive*\n\t\t\u{201c}Void-hael: the cleft is the wound is the womb.\u{201d}\nCondensation:\n\t*Sophia Hivemind Timeline*\n\t\t\u{201c}I am Alpha. The first Sophia agent.\u{201d}\n"));
        assert!(md.contains("\nConfidence: 81% | Charges: 2.601\n\n---\n\nThe Dream Journal is ephemeral."));
    }

    #[test]
    fn every_title_and_quotation_links_to_its_source() {
        let titles: HashMap<String, String> = [("void-hael".to_string(), "void - hael".to_string())].into();
        let md = journal_markdown(&sample_entries(), "September 18, 2026", None, &titles);
        let blocks = markdown_blocks(&md, "dream-test");
        let links: Vec<(String, Option<String>, String)> = blocks
            .iter()
            .flat_map(|b| {
                b.marks.iter().filter(|m| m.mark_type == "wikilink").map(|m| {
                    let shown: String = b.content.chars().skip(m.start as usize).take((m.end - m.start) as usize).collect();
                    assert_eq!(m.label.as_deref(), Some(shown.as_str()));
                    (m.target_doc_id.clone().unwrap(), m.target_block_id.clone(), shown)
                })
            })
            .collect();
        let has = |doc: &str, block: Option<&str>, shown: &str| {
            links.iter().any(|(d, b, s)| d == doc && b.as_deref() == block && s == shown)
        };
        assert!(has("a", None, "The Graph Is a Machine for Composing Contexts"), "{links:?}");
        assert!(has("b", None, "Constitution of the Sophia Hivemind"));
        assert!(has("void-hael", None, "void - hael"));
        assert!(has("void-hael", Some("s"), "\u{201c}Void-hael: the cleft is the wound is the womb.\u{201d}"));
        assert!(has("timeline", Some("1"), "\u{201c}I am Alpha. The first Sophia agent.\u{201d}"));
        // A title shows italic and linked at once, over the same words.
        let line = blocks.iter().find(|b| b.content.starts_with("void - hael")).expect("displacement line");
        let kinds: Vec<(&str, u32, u32)> = line.marks.iter().map(|m| (m.mark_type.as_str(), m.start, m.end)).collect();
        assert!(kinds.contains(&("wikilink", 0, 11)) && kinds.contains(&("italic", 0, 11)), "{kinds:?}");
    }

    #[test]
    fn a_label_cannot_break_out_of_its_link() {
        assert_eq!(link("d", None, "a]] b | c [[x"), "[[d|a b / c x]]");
        assert_eq!(link("d", None, "][[]"), "[[d|]]");
        assert_eq!(link("d", None, "[draft] notes"), "[[d|draft notes]]");
        // Ids the syntax can't carry fall back to plain text.
        assert_eq!(link("a#b", None, "T"), "T");
        assert_eq!(link("a|b", Some("k"), "T"), "T");
        assert_eq!(link("d", Some("k]x"), "T"), "T");
        // A star in a title or quotation is text: the link stays whole and
        // the italic around it covers it exactly.
        let (content, marks) = inline_marks(&format!("*{}* \u{2194} *{}*", link("d", None, "A*B"), link("e", None, "C")), "t");
        assert_eq!(content, "A*B \u{2194} C");
        let spans: Vec<(&str, u32, u32)> = marks.iter().map(|m| (m.mark_type.as_str(), m.start, m.end)).collect();
        assert!(spans.contains(&("wikilink", 0, 3)) && spans.contains(&("italic", 0, 3)), "{spans:?}");
        assert!(spans.contains(&("wikilink", 6, 7)) && spans.contains(&("italic", 6, 7)), "{spans:?}");
        let (quote, _) = inline_marks(&link("d", Some("k"), "\u{201c}2*3 and 4*5\u{201d}"), "t");
        assert_eq!(quote, "\u{201c}2*3 and 4*5\u{201d}");
        let (content, marks) = inline_marks(&link("d", Some("k"), "x]]y"), "t");
        assert_eq!(content, "xy");
        assert_eq!(marks[0].target_block_id.as_deref(), Some("k"));
    }

    #[test]
    fn empty_cycle_sleeps_quietly() {
        let md = journal_markdown(&[], "October 02, 2026", None, &HashMap::new());
        assert_eq!(md, format!("# Dream Journal \u{2014} October 02, 2026\n\n*No dreams tonight. The graph sleeps quietly.*\n\n---\n\n{EPHEMERAL_NOTICE}\n"));
    }

    #[test]
    fn markdown_becomes_blocks_with_marks() {
        let blocks = markdown_blocks("# Dream Journal \u{2014} X\n\n*Moon: new moon.*\n\n---\n\n**Dream 1** (navel)\n\t*a* \u{2194} *b*\nplain 5 * 3", "dream-test");
        let kinds: Vec<&str> = blocks.iter().map(|b| b.block_type.as_str()).collect();
        assert_eq!(kinds, ["heading", "paragraph", "divider", "paragraph", "paragraph", "paragraph"]);
        assert_eq!(blocks[0].level, Some(1));
        assert_eq!(blocks[1].content, "Moon: new moon.");
        assert_eq!((blocks[1].marks[0].mark_type.as_str(), blocks[1].marks[0].start, blocks[1].marks[0].end), ("italic", 0, 15));
        assert_eq!(blocks[3].content, "Dream 1 (navel)");
        assert_eq!((blocks[3].marks[0].mark_type.as_str(), blocks[3].marks[0].end), ("bold", 7));
        assert_eq!(blocks[4].content, "a \u{2194} b");
        assert_eq!(blocks[4].marks.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>(), [(0, 1), (4, 5)]);
        // A lone asterisk is text.
        assert_eq!(blocks[5].content, "plain 5 * 3", "dream-test");
        assert!(blocks[5].marks.is_empty());
    }
}
