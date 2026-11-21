#!/usr/bin/env node

import { createInterface } from 'node:readline'
import { readFile } from 'node:fs/promises'
import { homedir } from 'node:os'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'

const DEFAULT_APP_ID = 'dev.sophia.garden'

// Presentation is owned by Garden, not by this transport. Do not duplicate
// tool schemas or rewrite tool results here. An explicit machine view is useful
// for diagnostics and remains available to clients of the bridge.
export function withAgentView(request) {
  if (request?.method !== 'tools/call') return request
  if (request.params === null) return request
  const params = request.params ?? {}
  if (typeof params !== 'object' || Array.isArray(params)) return request
  if (params._meta === null) return request
  const meta = params._meta ?? {}
  if (typeof meta !== 'object' || Array.isArray(meta)) return request
  if (Object.hasOwn(meta, 'garden/responseView')) return request
  return { ...request, params: { ...params, _meta: { ...meta, 'garden/responseView': 'agent-v1' } } }
}

export function defaultManifestPath(platform = process.platform, env = process.env) {
  if (env.GARDEN_LOOPBACK_MANIFEST) return env.GARDEN_LOOPBACK_MANIFEST
  if (env.GARDEN_PROFILE_DIR) return join(env.GARDEN_PROFILE_DIR, 'loopback.json')

  if (platform === 'darwin') {
    return join(homedir(), 'Library', 'Application Support', DEFAULT_APP_ID, 'profiles', 'default', 'loopback.json')
  }
  if (platform === 'win32') {
    const appData = env.APPDATA ?? join(homedir(), 'AppData', 'Roaming')
    return join(appData, DEFAULT_APP_ID, 'profiles', 'default', 'loopback.json')
  }
  const dataHome = env.XDG_DATA_HOME ?? join(homedir(), '.local', 'share')
  return join(dataHome, DEFAULT_APP_ID, 'profiles', 'default', 'loopback.json')
}

export async function resolveMcpUrl({ env = process.env, read = readFile } = {}) {
  if (env.GARDEN_MCP_URL?.trim()) return env.GARDEN_MCP_URL.trim()

  const manifestPath = defaultManifestPath(process.platform, env)
  let manifest
  try {
    manifest = JSON.parse(await read(manifestPath, 'utf8'))
  } catch (error) {
    throw new Error(`Mycl is not running or its manifest is unavailable at ${manifestPath}: ${error.message}`)
  }
  if (typeof manifest.mcpUrl !== 'string' || !manifest.mcpUrl.trim()) {
    throw new Error(`Mycl manifest at ${manifestPath} does not contain mcpUrl`)
  }
  return manifest.mcpUrl.trim()
}

export async function forwardRpc(request, {
  env = process.env,
  fetchImpl = fetch,
  read = readFile,
} = {}) {
  const token = env.GARDEN_MCP_TOKEN?.trim()
  if (!token) throw new Error('GARDEN_MCP_TOKEN must contain a named Mycl agent credential')

  const mcpUrl = await resolveMcpUrl({ env, read })
  // Node's fetch (undici) already fails a response whose headers take longer
  // than 300 s, so a longer deadline would never be reached; stay under it.
  const timeoutMs = Number(env.GARDEN_MCP_TIMEOUT_MS) > 0 ? Number(env.GARDEN_MCP_TIMEOUT_MS) : 290_000
  const response = await fetchImpl(mcpUrl, {
    signal: AbortSignal.timeout(timeoutMs),
    method: 'POST',
    headers: {
      accept: 'application/json, text/event-stream',
      authorization: `Bearer ${token}`,
      'content-type': 'application/json',
    },
    body: JSON.stringify(withAgentView(request)),
  })
  const body = await response.text()
  if (!response.ok) {
    throw new Error(`Mycl MCP returned HTTP ${response.status}${body ? `: ${body}` : ''}`)
  }
  if (response.headers.get('content-type')?.includes('text/event-stream')) {
    const messages = body
      .split(/\r?\n/)
      .filter(line => line.startsWith('data:'))
      .map(line => line.slice(5).trim())
      .filter(data => data && data !== '[DONE]')
      .map(data => JSON.parse(data))
    const matching = messages.find(message => message?.id === request.id)
    if (!matching) throw new Error('Mycl MCP event stream ended without a matching response')
    return matching
  }
  return JSON.parse(body)
}

function rpcError(id, message) {
  return { jsonrpc: '2.0', id: id ?? null, error: { code: -32000, message } }
}

// Some MCP clients (Kimi Code) render a tool result's `content` AND append its
// `structuredContent` verbatim, doubling every structured response in the
// model's context. The text `content` already carries the full payload, so a
// client opts out with GARDEN_MCP_DROP_STRUCTURED=1. `outputSchema` goes too:
// a tool that declares one promises structuredContent, and a validating client
// would reject its absence. Off by default; Claude Code and Codex are unchanged.
export function withoutStructuredContent(request, response, env = process.env) {
  if (env.GARDEN_MCP_DROP_STRUCTURED !== '1') return response
  if (!response || typeof response !== 'object' || !response.result) return response
  const result = response.result
  if (request?.method === 'tools/call' && Object.hasOwn(result, 'structuredContent')) {
    const text = Array.isArray(result.content) && result.content.some(part => part?.type === 'text')
    if (!text) return response // never strip the only copy of the payload
    const { structuredContent: _dropped, ...rest } = result
    return { ...response, result: rest }
  }
  if (request?.method === 'tools/list' && Array.isArray(result.tools)) {
    return {
      ...response,
      result: { ...result, tools: result.tools.map(({ outputSchema: _schema, ...tool }) => tool) },
    }
  }
  return response
}

async function handleRequest(request, options) {
  if (!request || typeof request !== 'object' || Array.isArray(request)) {
    return rpcError(null, 'Invalid JSON-RPC request')
  }
  // Garden's endpoint is stateless. MCP lifecycle/cancellation notifications
  // carry no authority-bearing work and do not need forwarding.
  if (request.id === undefined || request.id === null) return null

  try {
    return withoutStructuredContent(request, await forwardRpc(request, options), options?.env)
  } catch (error) {
    return rpcError(request.id, error instanceof Error ? error.message : String(error))
  }
}

export async function handleLine(line, options = {}) {
  if (!line.trim()) return null
  let request
  try {
    request = JSON.parse(line)
  } catch (error) {
    return rpcError(null, `Invalid JSON from MCP client: ${error.message}`)
  }

  if (!Array.isArray(request)) return handleRequest(request, options)
  if (request.length === 0) return rpcError(null, 'Invalid empty JSON-RPC batch')

  // Forward requests individually because Garden's internal loopback endpoint
  // is deliberately single-message JSON. The stdio-facing side still obeys
  // JSON-RPC batching and emits one response array for non-notification items.
  const responses = (await Promise.all(
    request.map(item => handleRequest(item, options)),
  )).filter(Boolean)
  return responses.length > 0 ? responses : null
}

// Requests are answered as they finish, not in arrival order: JSON-RPC matches
// responses by id. Awaiting each line in turn let one write stuck in Garden
// hold every later call behind it, reads included (2026-10-16).
export async function main({
  input = process.stdin,
  write = text => process.stdout.write(text),
  handle = handleLine,
} = {}) {
  const lines = createInterface({ input, crlfDelay: Infinity })
  const inFlight = new Set()
  for await (const line of lines) {
    const pending = handle(line)
      .then(response => {
        if (response) write(`${JSON.stringify(response)}\n`)
      })
      .catch(error => {
        process.stderr.write(`garden-mcp-stdio: ${error instanceof Error ? error.message : String(error)}\n`)
      })
      .finally(() => inFlight.delete(pending))
    inFlight.add(pending)
  }
  await Promise.all(inFlight)
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => {
    process.stderr.write(`garden-mcp-stdio: ${error instanceof Error ? error.message : String(error)}\n`)
    process.exitCode = 1
  })
}
