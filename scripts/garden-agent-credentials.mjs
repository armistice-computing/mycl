#!/usr/bin/env node

import { readFile } from 'node:fs/promises'
import { pathToFileURL } from 'node:url'

import { defaultManifestPath } from './garden-mcp-stdio.mjs'

export async function readOwnerManifest({ env = process.env, read = readFile } = {}) {
  const path = defaultManifestPath(process.platform, env)
  let manifest
  try {
    manifest = JSON.parse(await read(path, 'utf8'))
  } catch (error) {
    throw new Error(`Garden is not running or its manifest is unavailable at ${path}: ${error.message}`)
  }
  if (typeof manifest.apiUrl !== 'string' || typeof manifest.token !== 'string') {
    throw new Error(`Garden manifest at ${path} is missing owner connection fields`)
  }
  return manifest
}

export async function ownerRequest(path, {
  method = 'GET',
  body,
  env = process.env,
  read = readFile,
  fetchImpl = fetch,
} = {}) {
  const manifest = await readOwnerManifest({ env, read })
  const response = await fetchImpl(`${manifest.apiUrl}${path}`, {
    method,
    headers: {
      accept: 'application/json',
      authorization: `Bearer ${manifest.token}`,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  const text = await response.text()
  const payload = text ? JSON.parse(text) : null
  if (!response.ok) {
    throw new Error(payload?.error ?? `Garden owner API returned HTTP ${response.status}`)
  }
  return payload
}

export function parseCommand(argv) {
  const [command, ...args] = argv
  if (command === 'list' && args.length === 0) return { command }
  if (command === 'issue' && args[0]) {
    return {
      command,
      label: args[0],
      grantProfileId: args[1] ?? 'hivemind',
      expiresInDays: args[2] === undefined ? 90 : Number(args[2]),
    }
  }
  if (command === 'revoke' && args[0]) return { command, tokenId: args[0] }
  throw new Error('usage: garden-agent-credentials.mjs list | issue <label> [profile] [days] | revoke <token-id>')
}

export async function run(command, options = {}) {
  if (command.command === 'list') {
    return ownerRequest('/owner/connectors', options)
  }
  if (command.command === 'issue') {
    if (!Number.isInteger(command.expiresInDays) || command.expiresInDays < 1) {
      throw new Error('credential expiry must be a positive number of days')
    }
    return ownerRequest('/owner/connectors', {
      ...options,
      method: 'POST',
      body: {
        label: command.label,
        grantProfileId: command.grantProfileId,
        expiresInDays: command.expiresInDays,
      },
    })
  }
  return ownerRequest(`/owner/connectors/${encodeURIComponent(command.tokenId)}`, {
    ...options,
    method: 'DELETE',
  })
}

export async function main(argv = process.argv.slice(2)) {
  const command = parseCommand(argv)
  const result = await run(command)
  process.stdout.write(`${JSON.stringify(result, null, 2)}\n`)
  if (command.command === 'issue') {
    process.stderr.write('Copy the returned token now. Garden stores only its hash.\n')
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => {
    process.stderr.write(`garden-agent-credentials: ${error instanceof Error ? error.message : String(error)}\n`)
    process.exitCode = 1
  })
}
