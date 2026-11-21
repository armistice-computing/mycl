import assert from 'node:assert/strict'
import test from 'node:test'

import { defaultManifestPath, forwardRpc, handleLine, main, resolveMcpUrl, withAgentView, withoutStructuredContent } from '../garden-mcp-stdio.mjs'

test('agent presentation is transport metadata, never a tool argument', () => {
  const request = { id: 1, method: 'tools/call', params: { name: 'recall', arguments: { limit: 8 }, _meta: { trace: 'keep' } } }
  const result = withAgentView(request)
  assert.deepEqual(result.params._meta, { trace: 'keep', 'garden/responseView': 'agent-v1' })
  assert.deepEqual(result.params.arguments, { limit: 8 })
  assert.deepEqual(request.params._meta, { trace: 'keep' })
  const machine = { ...request, params: { ...request.params, _meta: { 'garden/responseView': 'machine' } } }
  assert.equal(withAgentView(machine), machine)
})

test('bridge does not repair malformed requests or rewrite non-call methods', () => {
  for (const request of [
    { method: 'tools/call', params: null },
    { method: 'tools/call', params: { _meta: null } },
    { method: 'tools/call', params: [] },
    { method: 'tools/list', params: {} },
    { method: 'initialize', params: {} },
  ]) assert.equal(withAgentView(request), request)
})

test('forwarding opts in without altering response bodies or errors', async () => {
  const response = { jsonrpc: '2.0', id: 3, error: { code: -32602, message: 'bad parameter' } }
  const result = await forwardRpc({ id: 3, method: 'tools/call', params: { name: 'recall', arguments: {} } }, {
    env: { GARDEN_MCP_TOKEN: 'test', GARDEN_MCP_URL: 'http://127.0.0.1/mcp' },
    fetchImpl: async (_url, options) => {
      assert.equal(JSON.parse(options.body).params._meta['garden/responseView'], 'agent-v1')
      return new Response(JSON.stringify(response))
    },
  })
  assert.deepEqual(result, response)
})

test('manifest discovery honors explicit profile directory', () => {
  assert.equal(
    defaultManifestPath('darwin', { GARDEN_PROFILE_DIR: '/tmp/garden-profile' }),
    '/tmp/garden-profile/loopback.json',
  )
})

test('URL discovery reads only mcpUrl from the current manifest', async () => {
  const url = await resolveMcpUrl({
    env: { GARDEN_LOOPBACK_MANIFEST: '/tmp/loopback.json' },
    read: async () => JSON.stringify({ mcpUrl: 'http://127.0.0.1:4123/mcp', token: 'owner-secret' }),
  })
  assert.equal(url, 'http://127.0.0.1:4123/mcp')
})

test('RPC forwarding presents the named token, never the manifest token', async () => {
  let observed
  const result = await forwardRpc(
    { jsonrpc: '2.0', id: 7, method: 'tools/list', params: {} },
    {
      env: {
        GARDEN_LOOPBACK_MANIFEST: '/tmp/loopback.json',
        GARDEN_MCP_TOKEN: 'named-agent-token',
      },
      read: async () => JSON.stringify({ mcpUrl: 'http://127.0.0.1:4123/mcp', token: 'owner-secret' }),
      fetchImpl: async (url, options) => {
        observed = { url, options }
        return new Response(JSON.stringify({ jsonrpc: '2.0', id: 7, result: { tools: [] } }))
      },
    },
  )

  assert.equal(observed.url, 'http://127.0.0.1:4123/mcp')
  assert.equal(observed.options.headers.authorization, 'Bearer named-agent-token')
  assert.equal(observed.options.headers.accept, 'application/json, text/event-stream')
  assert.doesNotMatch(JSON.stringify(observed), /owner-secret/)
  assert.deepEqual(result.result, { tools: [] })
})

test('JSON-RPC batches return responses while consuming notifications', async () => {
  const called = []
  const result = await handleLine(JSON.stringify([
    { jsonrpc: '2.0', id: 1, method: 'initialize', params: {} },
    { jsonrpc: '2.0', method: 'notifications/initialized' },
    { jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} },
  ]), {
    env: {
      GARDEN_LOOPBACK_MANIFEST: '/tmp/loopback.json',
      GARDEN_MCP_TOKEN: 'named-agent-token',
    },
    read: async () => JSON.stringify({ mcpUrl: 'http://127.0.0.1:4123/mcp' }),
    fetchImpl: async (_url, options) => {
      const request = JSON.parse(options.body)
      called.push(request.id)
      return new Response(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: {} }))
    },
  })

  assert.deepEqual(called, [1, 2])
  assert.deepEqual(result.map(response => response.id), [1, 2])
})

test('finite event-stream responses are decoded by request id', async () => {
  const result = await forwardRpc(
    { jsonrpc: '2.0', id: 4, method: 'tools/list', params: {} },
    {
      env: {
        GARDEN_LOOPBACK_MANIFEST: '/tmp/loopback.json',
        GARDEN_MCP_TOKEN: 'named-agent-token',
      },
      read: async () => JSON.stringify({ mcpUrl: 'http://127.0.0.1:4123/mcp' }),
      fetchImpl: async () => new Response(
        'event: message\ndata: {"jsonrpc":"2.0","id":4,"result":{"tools":[]}}\n\n',
        { headers: { 'content-type': 'text/event-stream' } },
      ),
    },
  )

  assert.deepEqual(result.result, { tools: [] })
})

test('notifications are consumed without calling Garden', async () => {
  let called = false
  const result = await handleLine(
    JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }),
    { fetchImpl: async () => { called = true } },
  )
  assert.equal(result, null)
  assert.equal(called, false)
})

test('missing named token returns a protocol error without exiting', async () => {
  const result = await handleLine(
    JSON.stringify({ jsonrpc: '2.0', id: 9, method: 'initialize', params: {} }),
    { env: {}, read: async () => '{}' },
  )
  assert.equal(result.id, 9)
  assert.match(result.error.message, /GARDEN_MCP_TOKEN/)
})

test('structuredContent is dropped only for clients that opt in', () => {
  const call = { id: 1, method: 'tools/call', params: { name: 'value' } }
  const response = { jsonrpc: '2.0', id: 1, result: { content: [{ type: 'text', text: '{"a":1}' }], structuredContent: { a: 1 } } }
  assert.equal(withoutStructuredContent(call, response, {}), response, 'default: untouched')
  const dropped = withoutStructuredContent(call, response, { GARDEN_MCP_DROP_STRUCTURED: '1' })
  assert.deepEqual(dropped.result, { content: [{ type: 'text', text: '{"a":1}' }] })
  assert.ok(response.result.structuredContent, 'input not mutated')

  const onlyStructured = { id: 2, result: { content: [], structuredContent: { a: 1 } } }
  assert.equal(withoutStructuredContent(call, onlyStructured, { GARDEN_MCP_DROP_STRUCTURED: '1' }), onlyStructured,
    'never strips the only copy of the payload')

  const error = { id: 3, error: { code: -32000, message: 'x' } }
  assert.equal(withoutStructuredContent(call, error, { GARDEN_MCP_DROP_STRUCTURED: '1' }), error)
})

test('tools/list drops outputSchema when structuredContent is dropped', () => {
  const list = { id: 4, method: 'tools/list' }
  const response = { id: 4, result: { tools: [{ name: 'value', inputSchema: {}, outputSchema: { type: 'object' } }, { name: 'recall', inputSchema: {} }] } }
  const stripped = withoutStructuredContent(list, response, { GARDEN_MCP_DROP_STRUCTURED: '1' })
  assert.deepEqual(stripped.result.tools, [{ name: 'value', inputSchema: {} }, { name: 'recall', inputSchema: {} }])
  assert.equal(withoutStructuredContent(list, response, {}), response)
})

test('a stuck call does not hold the calls behind it (2026-10-16)', async () => {
  const { PassThrough } = await import('node:stream')
  const input = new PassThrough()
  const written = []
  let releaseSlow
  const slow = new Promise(resolve => { releaseSlow = resolve })
  const handle = async line => {
    const { id } = JSON.parse(line)
    if (id === 1) await slow
    else releaseSlow()
    return { jsonrpc: '2.0', id, result: {} }
  }
  const done = main({ input, write: text => written.push(JSON.parse(text).id), handle })
  input.write('{"jsonrpc":"2.0","id":1,"method":"tools/call"}\n')
  input.write('{"jsonrpc":"2.0","id":2,"method":"tools/call"}\n')
  input.end()
  await done
  assert.deepEqual(written, [2, 1])
})

test('every forward carries a deadline', async () => {
  let signal
  const fetchImpl = async (_url, init) => {
    signal = init.signal
    return new Response('{"jsonrpc":"2.0","id":1,"result":{}}', { status: 200, headers: { 'content-type': 'application/json' } })
  }
  const read = async () => JSON.stringify({ mcpUrl: 'http://127.0.0.1:1/mcp' })
  await forwardRpc({ jsonrpc: '2.0', id: 1, method: 'tools/list' }, {
    env: { GARDEN_MCP_TOKEN: 't', GARDEN_MCP_TIMEOUT_MS: '5000' }, fetchImpl, read,
  })
  assert.ok(signal instanceof AbortSignal, 'the fetch is given an abort signal')
})
