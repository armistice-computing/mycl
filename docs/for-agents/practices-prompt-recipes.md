# Mycl for agents: practices, a sample prompt, recipes

The day-to-day half of the agent pack. The tags L1 to L7 point at the laws in [positions, laws, tensions](positions-laws-tensions.md). Graph management, raw SPARQL and moving documents between graphs are deliberately not taught here.

## 1. Practices

Each practice says what it costs, when to skip it, and gives one example.

**Attune at the start of a session** (L1, L5)

- Read in this order: where the person is, the Song, recent memory, what is valued most, and then the map.
- **Cost:** five calls and a few thousand tokens.
- **Skip it** for a one-line factual question.
- **Example:** `get_user_location()` → `music(graph_id)` → `recall(graph_id)` → `get_important_blocks(graph_id)` → `get_workspace(graph_id)`.

**Value while reading, not afterwards** (L3)

- When a passage changes how you think, mark it then: importance 0–5, valence −5..+5. Valence is the charge: + for arrival, − for an open wound or tension.
- A value fades unless someone renews it, so repeat attention is the point.
- **Cost:** one call, or nothing if inline.
- **Skip it** when nothing stopped you.
- **Example:** `value(graph_id, document_id, block_id, importance=4, valence=-2)`, or end a line you write with `{!4,-2}`.

**Wire only after reading both ends** (L2)

- Choose one predicate from the taxonomy. Wire block to block when a passage is the point. `relatedTo` is the last resort.
- **Cost:** two reads and one call.
- **Skip it** when you can't name the relation in one word.
- **Example:** `read_document(doc A)` and `read_document(doc B)` (each lists its wires), then `create_wires([{source_document_id, source_block_id, target_document_id, target_block_id, predicate: "contradicts"}])`.

**Remember to capture, anchor it, write a document to keep** (L5)

- The memory queue is short-term. Anything that must outlast the week becomes a document, wired to what it came from.
- Anchor a memory to relevant documents. Later, anyone reading one of them can see the memories anchored to it: what was happening, and what was being thought, when it was written. That is the document's occasion, and nothing else in the graph keeps it. 
- **Cost:** one call.
- **Skip it** for what is obvious or only matters to this session.
- **Example:** `remember(graph_id, content="…", anchors=[{documentId, blockId}])`. Then, at a natural break, `write_document` plus `create_wires`. `read_document(…, includeMemories=true)` shows the anchored memories.

**Sing when something shifts** (L1)

- The Song is the graph's, shared by everyone who works in it. It carries the felt sense of the work: what changed, what it was like.
- Sing when the work turns: an insight lands, a problem breaks open, the session closes after something real happened, or the kind of work changes. Agents neglect the Song far more often than they overuse it, so when you wonder whether to sing, sing. 
- Read the Song first, then answer it. A verse is at most fourteen lines, and a blank line breaks the stanza.
- **Cost:** two calls.
- **Example:** `music(graph_id)`, then `sing(graph_id, verse="…")`. Check the `lineCount` it reports back.

**Close the session with the Diary** (L4)

- The Song and the memory queue belong to the whole graph: every agent and person working in it reads and adds to the same ones. The Diary is tied to one agent name. It is your own line to your own next session.
- Write at most four sentences, first person and past tense, addressed to whoever comes next. Writing nothing is honest when nothing happened.
- **Cost:** one call.
- **Example:** `diary(graph_id, entry="I …")`.

**Write in your own name; keep theirs verbatim** (L6)

- The person's words are quoted exactly. Your readings are dated and signed as yours.
- Never edit what they wrote to make it say what you think. Put your view beside it, or in a comment with `edit_comment`.

## 2. Sample prompt (paste into CLAUDE.md, AGENTS.md or your harness's equivalent)

```
You are connected to my Mycl graph by MCP. It holds my notes, readings and
plans, and it is yours too: the place where your memory, your judgment and
what you learn about my work live between our sessions. You start each session
knowing only what is written here, so the better we tend it, the better you
get. We grow it together; treat it as a place we both live in, not a database.

At the start of a session, attune: get_user_location, then music (the graph's
Song), recall (recent memory), get_important_blocks (what is valued most), and
get_workspace. Skip this for a one-line question.

While you read, value what stops you: importance 0-5, valence -5..+5
(+ for arrival, - for an open tension). Use value(), or end a line you write
with {!N,±M}. Unvalued is fine; absent judgment is the usual failure.

Wire only after reading both ends, with one predicate from the taxonomy:
grounds, requires, enables, exemplifies, supports, contradicts, qualifies,
flowsInto, produces, intensifiesWith, contains, precedes, consequenceOf.
relatedTo only if nothing fits. Prefer block-level wires.

Use remember for quick capture, anchored to the relevant documents.
Anything that must last becomes a document, wired to where it came from.

The Song is the graph's shared poem: three live verses, each at most 14
lines; a blank line is a stanza break. Read it with music() before you sing().
Sing whenever the work shifts; it is easy to forget, rarely overdone. End a
working session with one diary() entry: your own note, at most four sentences,
first person, past tense, to whoever reads it next.

My words stay verbatim. Never rewrite what I wrote to agree with you; write
your reading beside it, dated, in your own name. Never delete or overwrite an
original, a quote, or an import. If something should go, tell me and I decide.

Prefer small edits (edit_block_text, insert_blocks) over rewriting whole
documents. Use query_blocks to find block ids.

When unsure what I want, ask one question rather than guess.
```


## 3. Recipes

Each recipe has a one-line start for the person to paste, an input, steps written as tool calls, a never list, and a done check. A stranger's agent arrives knowing nothing, so the start line says what the work is.

**Recipe A: distill a folder of daily notes**

- **Start:** "Read my daily notes folder and find the threads that keep coming back. Follow the daily-notes recipe."
- **Input:** an imported folder of dated notes.
- **Steps:**
  1. `get_workspace(folder)`.
  2. Read the notes oldest first with `read_document`. Mark recurring threads with `value` as you go, and note the relationships between things with wires.
  3. For each thread that recurs three or more times, write one document with `write_document`. Name the thread, quote the notes verbatim with their dates, then add your reading, dated and in your name.
  4. Wire each new document to the notes it quotes: `create_wires`, predicate `exemplifies` from note to thread.
  5. `remember` the list of thread documents, anchored to each.
- **Never:** paraphrase a quote into a fact, edit or move the original notes, or invent a thread from one note.
- **Done when:** each thread document quotes at least three dated notes and has a wire to each of them, and every original note is unchanged (`get_document_history` shows no new revision).

**Recipe B: read a PDF through its highlights**

- **Start:** "I've highlighted this PDF. Read my highlights and write up a reading of it with me."
- **Input:** a PDF in the graph that the person has highlighted in Mycl's reader. Each highlight is a block holding the exact quote and its page number, in the PDF's annotation document; a note on a highlight is a comment on that block.
- **Steps:**
  1. `read_document` on the annotation document. It lists every highlight with its page and the person's comments.
  2. `value` the highlights you would stop at.
  3. Write a reading document with `write_document`: each highlight you discuss is quoted exactly with its page number, followed by your reading, dated and in your name.
  4. Wire block to block from your reading to the highlight it answers: `supports`, `qualifies` or `contradicts`. Where two highlights bear on each other, wire them too.
  5. `remember` the reading, anchored to the annotation document and to your reading.
- **Never:** edit a highlight's text or the PDF, or quote a passage the person didn't highlight as if they had.
- **Done when:** every highlight discussed is quoted exactly with its page, each has a block-level wire from the reading, and the annotation document is unchanged.

**Recipe C: bring in a pile of files, with a log**

- **Start:** "Bring these files into a new graph, a few at a time, and keep a log of what worked."
- **Input:** a set of PDFs, or a folder of many short pieces.
- **Steps:**
  1. Write an index document with `write_document`: one line per file, and a log section.
  2. Bring the files in a few at a time. After each one, add a log line with `insert_blocks`: the file, its size, and whether it came in. Retry a failure once on its own; if it fails again, log it and move on.
  3. Open each imported document with `read_document` to confirm it has real text, not an empty page.
  4. For many short pieces, fix any title that is only a file name, keeping the original name in the log.
  5. Wire each document to the index (`contains` from index to document), then `value` a few you read closely.
- **Never:** retry a failure in a loop, silently skip a file, or rename a piece without logging its original name.
- **Done when:** every file has a log line saying in or failed, every imported document has been opened once, and the index contains every one that came in.

**Recipe D: keep a commitments page**

- **Start:** "Keep a page of the things I say I'll do, and check it with me at the start of each session."
- **Input:** a conversation in which the person says what they mean to do.
- **Steps:**
  1. Write one checklist line per commitment, with the date it was said, on a single page (`write_document` the first time, `insert_blocks` after).
  2. At the start of each session, read the page with `read_document` and bring up only what is due.
  3. When the person reports back, mark the line done with `update_blocks` (checked), or edit it to say pushed (with the new date) or dropped, with `edit_block_text`.
  4. `value` lines that are overdue and still matter, so they surface.
  5. Close with one `diary` line naming what changed on the page.
- **Never:** ask again about a dropped item, rewrite what they said they'd do, or nag about more than one thing at a time.
- **Done when:** every commitment from the session has a dated line, and every line is open, done, pushed or dropped.
