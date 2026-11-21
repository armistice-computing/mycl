# Architecture

Mycl is one desktop program. Its binary is called `mycl`. The program opens a window, keeps the graphs on disk, and serves them on loopback (`127.0.0.1`) to the window and to agents. Nothing leaves the computer unless you ask for it: a web clip, a model download, a PDF runtime install.

This repository is the program without its window. The window is a web app in [mycl-shrubbery](https://github.com/armistice-computing/mycl-shrubbery); the build fetches it and packs it in.

Three ideas organize the code.

## 1. The app is the node

The process that draws the window also runs the graph node. At start it binds a port on loopback and writes `loopback.json` into the profile folder, with the address and a token. The window and every agent find the node through that file. When the app quits, the node is gone.

The same code builds without a window, as the `gardend` example (`--no-default-features --features headless`). Most tests use that build.

## 2. Documents are Yjs; the rest is derived

Each document is a Yjs document (the `yrs` crate), and the node is the only writer of the files. The window edits over a WebSocket room; agents edit through tools that apply the same kind of update. The folder tree of a graph is one more Yjs document.

From those the node fills an RDF store (Oxigraph) per graph: blocks, wires, folders, titles. SPARQL and wire queries read the store. Text search keeps its own projection of the documents. An optional embedding index adds search by meaning; its model is downloaded only when you turn it on.

Memory, the Song, the Diary and block values are small JSON stores beside the documents. Details: [docs/storage.md](docs/storage.md).

## 3. Agents come in through one door

Agents speak MCP to `POST /mcp` on the loopback port, with a bearer token. `mycl --mcp-stdio` is the same door for agents that want a stdio server. A token carries a grant profile: a list of tools, and the scopes those tools need. The tool list is one file, `src-tauri/src/mcp_tool_catalog.json`. Details: [docs/agents.md](docs/agents.md).

## The parts

All paths are under `src-tauri/src/`. Most of the code is flat files grouped by prefix.

| Prefix or folder | What it holds |
|---|---|
| `loopback_*` | The HTTP server (axum): routes, tokens, grant profiles, scopes. |
| `mcp_*` | The MCP door: the catalog, the dispatch to tools, the stdio bridge. |
| `crdt_engine/`, `document_*`, `workspace_*` | Yjs rooms, document and folder operations, history. |
| `rdf_*`, `sparql_*`, `wire_*` | The RDF store, how documents are written into it, queries, wires. |
| `search_*`, `semantic_*` | Text search and its projection, the embedding models and their index. |
| `geist_*`, `salience_*` | Memory, Song, Diary, block values and the important-blocks view. |
| `dreaming/`, `dreamer*` | The nightly dream: pairs of blocks handed to a program you choose. |
| `vault_*`, `pdf_*`, `web_*`, `artifact_*` | Ways in: whole vaults, PDFs, web clips, files. |
| `graph_archive_*`, `markdown_*` | Ways out: graph archives and markdown folders. |
| `terminal*` | The terminal panel's shell. |
| `onboarding_*` | The first graph, Start, unpacked from `resources/onboarding/`. |
| `emporium/`, `observatory/`, `hosted_*` | Inherited from Garden. Other modules still call into them (the shapes of the REST answers the window reads, the RDF term and diff code), so read them when a trace leads there. |

## Reading order

1. `src-tauri/src/lib.rs`: the module list, and `run()`.
2. `tauri_runtime.rs`: what happens at start.
3. `loopback_token_grants.rs`: who may call what.
4. `mcp_tool_catalog.json`, then `mcp_dispatch_registry.rs`: a tool from its name to its code.
5. `crdt_engine/rooms.rs`: a live document.

Then: [docs/building.md](docs/building.md), [docs/testing.md](docs/testing.md).
