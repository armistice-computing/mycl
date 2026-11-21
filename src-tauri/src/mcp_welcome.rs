//! What a connecting agent is told first: the `instructions` string of the MCP
//! `initialize` reply. It names the product's own words once, so a stranger's
//! agent meets the graph and not the people who built it. Text by Delta, with
//! Eschaton's vocabulary ruling of 2026-11-17: the product words stay, Geist
//! stays as a named idea, and nothing here speaks of a house.
//!
//! A sentence that names a tool is left out when the key cannot call it: a
//! connector must not be told to use what it was not given.

/// The server's name in `serverInfo`. The visible product name is not
/// settled; this one is only read by clients and logs.
pub(crate) const MCP_SERVER_NAME: &str = "mycl-local";

/// The welcome for a key that can call the tools `can_call` answers true for.
pub(crate) fn mcp_instructions(can_call: impl Fn(&str) -> bool) -> String {
    // The tools of `names` this key has, as "a", "a and b" or "a, b and c".
    let named = |names: &[&str]| -> Option<String> {
        let have: Vec<&str> = names.iter().copied().filter(|name| can_call(name)).collect();
        match have.as_slice() {
            [] => None,
            [one] => Some((*one).to_string()),
            [first @ .., last] => Some(format!("{} and {last}", first.join(", "))),
        }
    };
    let mut text = String::new();

    text.push_str(
        "Mycl is a local knowledge graph that people and agents write in together. \
         The graphs are kept on this computer.\n\n",
    );

    text.push_str(
        "What is here. A graph holds documents in folders. A document is made of blocks \
         (paragraphs, headings, list items), and each block has an id. Wires are typed links \
         between documents or between blocks: supports, contradicts, grounds, flowsInto and \
         others. Folders organize, and wires carry meaning.",
    );
    if can_call("create_wires") {
        text.push_str(
            " When one passage bears on another, wire them with the most specific predicate \
             that fits.",
        );
    }
    text.push_str(
        " Files such as PDFs, images and the originals of imports are artifacts, and a wire \
         can point at a line range of a file.\n\n",
    );

    let mut steps: Vec<String> = Vec::new();
    if can_call("list_graphs") {
        steps.push(if can_call("set_home_graph") {
            "If a tool says there is no home graph, call list_graphs, then set_home_graph."
                .to_string()
        } else {
            "If a tool says there is no home graph, call list_graphs and pass graph_id."
                .to_string()
        });
    }
    let mut finding: Vec<&str> = Vec::new();
    if can_call("get_workspace") {
        finding.push("get_workspace shows the folders and documents.");
    }
    if can_call("search_documents") {
        finding.push("search_documents finds documents by title.");
    }
    if can_call("search_blocks") {
        finding.push("search_blocks finds blocks by their text.");
    }
    if !finding.is_empty() {
        steps.push(finding.join(" "));
    }
    if can_call("read_document") {
        let mut step = "read_document returns a document with its wires.".to_string();
        if let Some(tools) = named(&["edit_block_text", "update_blocks", "insert_blocks"]) {
            step.push_str(&format!(
                " To edit, prefer {tools} to rewriting a whole document. Edits appear live \
                 in the person's window."
            ));
        }
        steps.push(step);
    }
    if !steps.is_empty() {
        text.push_str("How to start.\n");
        for (index, step) in steps.iter().enumerate() {
            text.push_str(&format!("{}. {step}\n", index + 1));
        }
        text.push('\n');
    }

    let mut geist: Vec<String> = Vec::new();
    if let Some(tools) = named(&["remember", "recall"]) {
        let mut line = format!("- Memory queue ({tools}): quick notes to find later.");
        if can_call("remember") {
            line.push_str(" A note that matters belongs in a document, wired to its context.");
        }
        geist.push(line);
    }
    if let Some(tools) = named(&["music", "sing"]) {
        let mut line = format!(
            "- Song ({tools}): a short poem for each graph that carries the felt sense of \
             the work."
        );
        if can_call("sing") {
            line.push_str(" Read it before you add a verse. A new verse retires the oldest.");
        }
        geist.push(line);
    }
    if let Some(tools) = named(&["diary", "read_diary"]) {
        geist.push(format!(
            "- Diary ({tools}): one dated line at the end of a session, for whoever picks \
             up the work next."
        ));
    }
    if let Some(tools) = named(&["value", "get_important_blocks"]) {
        geist.push(format!(
            "- Salience ({tools}): importance 0 to 5 and valence -5 to +5 on a block. \
             Values fade over time and weight search."
        ));
    }
    if !geist.is_empty() {
        text.push_str(
            "Geist. These are the graph's memory tools. Each keeps time, so what nobody tends \
             fades.\n",
        );
        for line in &geist {
            text.push_str(line);
            text.push('\n');
        }
        text.push('\n');
    }

    text.push_str(
        "The graph belongs to the person. Ask before you delete, move many documents, or \
         rewrite what someone else wrote.",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_key_is_told_every_part() {
        let text = mcp_instructions(|_| true);
        for part in [
            "Mycl is a local knowledge graph",
            "wire them with the most specific predicate",
            "1. If a tool says there is no home graph, call list_graphs, then set_home_graph.",
            "2. get_workspace shows the folders and documents. search_documents finds",
            "To edit, prefer edit_block_text, update_blocks and insert_blocks to rewriting",
            "Geist. These are the graph's memory tools.",
            "- Memory queue (remember and recall)",
            "- Song (music and sing)",
            "- Diary (diary and read_diary)",
            "- Salience (value and get_important_blocks)",
            "The graph belongs to the person.",
        ] {
            assert!(text.contains(part), "missing: {part}\n{text}");
        }
    }

    #[test]
    fn the_welcome_does_not_speak_of_the_house() {
        let text = mcp_instructions(|_| true);
        for word in ["Sophia", "house", "watch", "hand", "Cantor", "Garden", "Mycyl"] {
            assert!(!text.contains(word), "house word {word:?} in:\n{text}");
        }
    }

    #[test]
    fn a_read_only_key_is_not_told_to_write() {
        let read_only = [
            "list_graphs",
            "get_workspace",
            "search_documents",
            "search_blocks",
            "read_document",
            "recall",
            "music",
        ];
        let text = mcp_instructions(|name| read_only.contains(&name));
        for absent in [
            "set_home_graph",
            "wire them",
            "edit_block_text",
            "remember",
            "sing",
            "add a verse",
            "- Diary",
            "- Salience",
        ] {
            assert!(!text.contains(absent), "{absent:?} offered to a read-only key:\n{text}");
        }
        assert!(text.contains("call list_graphs and pass graph_id."));
        assert!(text.contains("- Memory queue (recall): quick notes to find later.\n"));
        assert!(text.contains("- Song (music): a short poem"));
    }

    #[test]
    fn every_tool_named_is_one_the_key_can_call() {
        // A key with one tool of each group: no sibling's name may appear.
        let has = ["search_blocks", "read_document", "insert_blocks", "recall", "music"];
        let text = mcp_instructions(|name| has.contains(&name));
        for absent in [
            "get_workspace",
            "search_documents",
            "edit_block_text",
            "update_blocks",
            "remember",
            "sing",
            "list_graphs",
        ] {
            assert!(!text.contains(absent), "{absent:?} named to a key without it:\n{text}");
        }
        assert!(text.contains("To edit, prefer insert_blocks to rewriting a whole document."));
    }

    #[test]
    fn a_key_with_no_tools_still_gets_the_two_plain_paragraphs() {
        let text = mcp_instructions(|_| false);
        assert!(text.starts_with("Mycl is a local knowledge graph"));
        assert!(!text.contains("How to start."));
        assert!(!text.contains("Geist."));
        assert!(text.ends_with("rewrite what someone else wrote."));
    }
}
