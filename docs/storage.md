# Storage

Everything Mycl keeps is in one profile folder. There is no database server and no account.

## Where

The profile is `profiles/default/` under the app data folder for the identifier `dev.sophia.garden`:

- macOS: `~/Library/Application Support/dev.sophia.garden/profiles/default/`
- Linux: `~/.local/share/dev.sophia.garden/profiles/default/`

`GARDEN_PROFILE_DIR` names another folder (`src-tauri/src/profile_paths.rs`).

## The profile

| Path | What it is |
|---|---|
| `loopback.json` | Written at every start: the node's address, port and owner token. Treat it as a password file. |
| `loopback-client-tokens.json` | The keys you gave to agents, with their grant profiles and expiry dates. |
| `loopback-audit.jsonl` | An audit log of loopback security events, such as keys issued and revoked. |
| `graphs/<graph-id>/` | One folder per graph. |
| `models/` | Downloaded models and the PDF runtime, if you installed them. |
| `jobs/` | Long jobs (imports, reindexing) and their results. |
| `onboarding-seeded.json` | Marks that the first graph, Start, was already unpacked. |
| `metadata.oxigraph/` | Profile-level RDF metadata. |

## A graph

| Path | What it is |
|---|---|
| `graph.json` | The graph's id and title. |
| `ydocs/workspace/` | The folder tree and the wires, as one Yjs document. |
| `ydocs/documents/<document-id>/update-v1.bin` | One document, as a Yjs update. The other files in that folder, such as `blocks.json`, are readings of it. |
| `documents/<document-id>/` | The document's record (`document.json`) and its history. |
| `store.oxigraph/` | The RDF store, filled from the Yjs documents. |
| `indexes/semantic/` | The embedding index, if you turned it on. |
| `artifacts/`, `images/` | Files you attached, and the originals of what you imported. |
| `memory/` | The memory queue and its archive. |
| `narrative/song.json` | The Song. |
| `diary/` | Diary entries, one file per writer. |
| `values/block-values.json` | Importance and valence for each rated block. |
| `dreaming/` | The dream's state, schedule, exclusions and settings. |

## What is the truth

The Yjs files under `ydocs/` are the documents. The RDF store and the indexes are derived from them, and the node can fill them again. Memory, Song, Diary and values are stores of their own and are not derived from anything.

To move a graph to another computer, export it from the app (a graph archive, or a folder of markdown). Copying a graph folder between running apps is not supported.

To back up everything, quit the app and copy the profile folder.
