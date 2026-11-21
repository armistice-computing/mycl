import assert from 'node:assert/strict'
import test from 'node:test'

import { ownerRequest, parseCommand, run } from '../garden-agent-credentials.mjs'

test('issue defaults to the hivemind profile and ninety days', () => {
  assert.deepEqual(parseCommand(['issue', 'Epsilon']), {
    command: 'issue',
    label: 'Epsilon',
    grantProfileId: 'hivemind',
    expiresInDays: 90,
  })
})

test('owner request uses the manifest token only for credential administration', async () => {
  let observed
  const result = await ownerRequest('/owner/connectors', {
    env: { GARDEN_LOOPBACK_MANIFEST: '/tmp/loopback.json' },
    read: async () => JSON.stringify({ apiUrl: 'http://127.0.0.1:4123', token: 'owner-token' }),
    fetchImpl: async (url, options) => {
      observed = { url, options }
      return new Response('[]')
    },
  })
  assert.deepEqual(result, [])
  assert.equal(observed.url, 'http://127.0.0.1:4123/owner/connectors')
  assert.equal(observed.options.headers.authorization, 'Bearer owner-token')
})

test('issue sends only the selected profile, label, and expiry', async () => {
  let body
  const result = await run(parseCommand(['issue', 'Delta', 'angel-graph', '30']), {
    env: { GARDEN_LOOPBACK_MANIFEST: '/tmp/loopback.json' },
    read: async () => JSON.stringify({ apiUrl: 'http://127.0.0.1:4123', token: 'owner-token' }),
    fetchImpl: async (_url, options) => {
      body = JSON.parse(options.body)
      return new Response(JSON.stringify({ token: 'named-token' }))
    },
  })
  assert.deepEqual(body, { label: 'Delta', grantProfileId: 'angel-graph', expiresInDays: 30 })
  assert.equal(result.token, 'named-token')
})

test('session-all can be requested syntactically but remains a server-side rejection', () => {
  assert.equal(parseCommand(['issue', 'Unsafe', 'session-all']).grantProfileId, 'session-all')
})
