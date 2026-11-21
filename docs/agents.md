# How an agent connects

An agent reaches a graph through MCP, over loopback, with a key you gave it. The app must be running: the app is the node.

## For a person

Open Settings, then Connected agents, then Connect an agent. The page makes a key, and prints the setup command for the agent you use. You do not need this page for anything else.

## What the page sets up

The agent's MCP configuration starts the app's own binary as a stdio server:

```
command: /Applications/Mycl.app/Contents/MacOS/mycl
args:    --mcp-stdio
env:     GARDEN_MCP_TOKEN=<the key>
```

`mycl --mcp-stdio` opens no window and no store. It reads `loopback.json` in the profile folder to find the running app, and passes each JSON-RPC line to `POST /mcp` with the key as a bearer token (`src-tauri/src/mcp_stdio.rs`). `GARDEN_MCP_URL` skips the file and names the address directly.

An agent that speaks HTTP MCP can call `/mcp` itself. The port changes at each start of the app, so read it from `loopback.json`.

## Keys and grant profiles

A key has a name, an expiry date and one grant profile. The profile fixes which tools the key may call, and the scopes those tools need on the REST routes. A call outside the profile is refused. The profiles are in `src-tauri/src/loopback_token_grants.rs`. The ones a person normally chooses:

| Profile | What it allows |
|---|---|
| `garden` (shown as "Mycl agent") | Read and write documents, folders, wires, memory, Song, Diary, values and files; SPARQL reads. The recommended one. |
| `read-only` | Reads only. |
| `developer` | Every tool, including the low-level ones. For testing. |

Keys are stored hashed in `loopback-client-tokens.json`. The owner token in `loopback.json` is for the window. Do not give it to an agent.

## The tools

`src-tauri/src/mcp_tool_catalog.json` holds every tool: its name, description and input schema. `mcp_dispatch_registry.rs` maps a name to its code. To add a tool, add it to the catalog, give it a handler in the registry, and add its name to the profiles that should have it. Tests fail if they disagree.

The server's `instructions` text, which an agent reads when it connects, describes the graph in a few paragraphs. The longer guidance for agents is in [for-agents](for-agents/).

## What an agent's write looks like

A tool that edits a document applies a Yjs update to the same room the window has open, so the edit appears in the window as it lands. Each write is recorded with its author (the key's name), which `read_document` returns as the document's chronicle.
