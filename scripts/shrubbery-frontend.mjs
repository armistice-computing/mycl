#!/usr/bin/env node

/**
 * Build adapter between Garden's native shell and its authoritative Shrubbery UI.
 *
 * Garden owns the Tauri/Rust runtime and the generated packaging directory.
 * Shrubbery owns every browser-facing source file. The lock file makes that
 * boundary reproducible instead of silently building whichever sibling branch
 * happens to be checked out.
 */

import { spawn, spawnSync } from 'node:child_process'
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  renameSync,
  writeFileSync,
} from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { randomUUID } from 'node:crypto'
import { readShrubberyLock, resolveShrubberyLock, gitAuthenticationArgs } from './resolve-shrubbery-lock.mjs'

const gardenRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const declaredLock = readShrubberyLock()
const mode = process.argv[2]

if (!['build', 'dev', 'preview'].includes(mode)) {
  console.error('Usage: node scripts/shrubbery-frontend.mjs <build|dev|preview>')
  process.exit(2)
}

function runGit(root, args, { fatal = true } = {}) {
  const result = spawnSync('git', ['-C', root, ...args], {
    encoding: 'utf8',
    env: { ...process.env, GIT_TERMINAL_PROMPT: '0' },
  })
  if (fatal && result.status !== 0) {
    process.stderr.write(result.stderr || result.error?.message || '')
    process.exit(result.status ?? 1)
  }
  return result
}

function isShrubberyCheckout(root) {
  return Boolean(root && existsSync(resolve(root, 'apps/organism/package.json')))
}

function gitCheckoutHead(root) {
  if (!root) return null
  // `git -C child rev-parse HEAD` walks upward and would otherwise mistake the
  // caller's Garden checkout for provenance on an action-materialized archive.
  if (!existsSync(resolve(root, '.git'))) return null
  const result = runGit(root, ['rev-parse', 'HEAD'], { fatal: false })
  return result.status === 0 ? result.stdout.trim() : null
}

function archiveCheckoutHead(root) {
  if (!isShrubberyCheckout(root)) return null
  const marker = resolve(root, '.shrubbery-source-revision')
  if (!existsSync(marker)) return null
  const revision = readFileSync(marker, 'utf8').trim()
  return process.env.SHRUBBERY_ARCHIVE_REVISION === revision ? revision : null
}

function checkoutHead(root) {
  return gitCheckoutHead(root) ?? archiveCheckoutHead(root)
}

function materializePinnedShrubbery() {
  const cacheRoot = resolve(
    process.env.SHRUBBERY_CACHE_DIR ||
      resolve(gardenRoot, 'src-tauri', 'target', 'shrubbery-ui'),
    lock.commit,
  )
  if (checkoutHead(cacheRoot) === lock.commit) return cacheRoot
  if (existsSync(cacheRoot)) {
    throw new Error(`Shrubbery cache ${cacheRoot} has unexpected provenance; inspect it or choose another SHRUBBERY_CACHE_DIR`)
  }
  mkdirSync(dirname(cacheRoot), { recursive: true })
  const temporaryRoot = mkdtempSync(`${cacheRoot}.tmp-`)

  console.log(`Materializing authoritative Shrubbery UI ${lock.commit.slice(0, 12)}…`)
  const localSource = lock.sourceRoot && runGit(lock.sourceRoot, ['cat-file', '-e', `${lock.commit}^{commit}`], { fatal: false }).status === 0
    ? lock.sourceRoot : null
  const credentialArgs = localSource ? [] : gitAuthenticationArgs()
  const commands = [
    ['init', '--quiet', temporaryRoot],
    ['-C', temporaryRoot, 'remote', 'add', 'origin', lock.repositoryUrl],
    [...credentialArgs, '-C', temporaryRoot, 'fetch', '--quiet', '--no-tags', '--depth=1', localSource || 'origin', lock.commit],
    ['-C', temporaryRoot, '-c', 'advice.detachedHead=false', 'checkout', '--quiet', '--detach', 'FETCH_HEAD'],
  ]
  for (const args of commands) {
    const result = spawnSync('git', args, {
      encoding: 'utf8',
      env: { ...process.env, GIT_TERMINAL_PROMPT: '0' },
    })
    if (result.status !== 0) {
      rmSync(temporaryRoot, { recursive: true, force: true })
      process.stderr.write(result.stderr || result.error?.message || '')
      console.error(
        `Unable to fetch resolved Shrubbery revision ${lock.commit} from ${localSource || lock.repository}. ` +
        'Authenticate gh or set SHRUBBERY_DIR to an exact local checkout.',
      )
      process.exit(result.status ?? 1)
    }
  }
  try {
    renameSync(temporaryRoot, cacheRoot)
  } catch (error) {
    rmSync(temporaryRoot, { recursive: true, force: true })
    // A concurrent resolver may have published the same immutable source.
    if (checkoutHead(cacheRoot) !== lock.commit) throw error
  }
  return cacheRoot
}

const explicitRoot = process.env.SHRUBBERY_DIR
const siblingRoot = resolve(gardenRoot, '..', 'mycl-shrubbery')
if (explicitRoot && !existsSync(resolve(explicitRoot, '.git')) && !isShrubberyCheckout(explicitRoot)) {
  throw new Error(`SHRUBBERY_DIR is not a Shrubbery checkout: ${explicitRoot}`)
}
const lock = resolveShrubberyLock(declaredLock, { localRoot: explicitRoot || siblingRoot })
console.log(`Shrubbery ${lock.requestedRevision} -> ${lock.commit} (${lock.resolution})`)
let shrubberyRoot

if (lock.resolution === 'local-main') {
  const sourceHead = gitCheckoutHead(lock.sourceRoot)
  const branch = runGit(lock.sourceRoot, ['symbolic-ref', '--quiet', '--short', 'HEAD'], { fatal: false }).stdout.trim()
  shrubberyRoot = sourceHead === lock.commit && branch === 'main'
    ? lock.sourceRoot : materializePinnedShrubbery()
} else if (explicitRoot) {
  shrubberyRoot = resolve(explicitRoot)
} else if (checkoutHead(siblingRoot) === lock.commit) {
  shrubberyRoot = siblingRoot
} else if (mode === 'build') {
  shrubberyRoot = materializePinnedShrubbery()
} else if (isShrubberyCheckout(siblingRoot)) {
  shrubberyRoot = siblingRoot
} else {
  console.error(
    `Shrubbery checkout not found. Set SHRUBBERY_DIR to a checkout of ${lock.repository} ` +
    `at ${lock.commit}.`,
  )
  process.exit(1)
}

const gitHead = gitCheckoutHead(shrubberyRoot)
if (lock.sourceRoot && resolve(shrubberyRoot) !== resolve(lock.sourceRoot)) {
  console.log(`Using isolated source at ${shrubberyRoot}; ${lock.sourceRoot} is unchanged.`)
}
if (!isShrubberyCheckout(shrubberyRoot)) {
  throw new Error(`Resolved source ${shrubberyRoot} does not contain apps/organism/package.json`)
}
const head = gitHead ?? archiveCheckoutHead(shrubberyRoot)
if (!head) {
  console.error(
    `Shrubbery source at ${shrubberyRoot} has neither Git provenance nor a ` +
    'matching immutable workflow archive marker.',
  )
  process.exit(1)
}
if (head !== lock.commit) {
  console.error(
    `Garden resolved Shrubbery ${lock.requestedRevision} to ${lock.commit}, but ${shrubberyRoot} is at ${head}. ` +
    'Check out the selected revision or change the explicit pin intentionally.',
  )
  process.exit(1)
}

const dirty = gitHead
  ? runGit(shrubberyRoot, ['status', '--porcelain']).stdout.trim()
  : ''
if (dirty && mode === 'build' && process.env.SHRUBBERY_ALLOW_DIRTY !== '1') {
  console.error(
    'Refusing to package a dirty Shrubbery checkout. Commit the UI change, or set ' +
    'SHRUBBERY_ALLOW_DIRTY=1 for a deliberately non-reproducible local build.',
  )
  process.exit(1)
}
if (dirty && mode !== 'build') {
  console.warn('Shrubbery has uncommitted changes; the development server will include them.')
}

function verifyBuildSource() {
  if (checkoutHead(shrubberyRoot) !== lock.commit) {
    throw new Error('Shrubbery HEAD changed during the build; refusing to publish mismatched provenance')
  }
  const currentDirty = gitHead ? runGit(shrubberyRoot, ['status', '--porcelain']).stdout.trim() : ''
  if (currentDirty && process.env.SHRUBBERY_ALLOW_DIRTY !== '1') {
    throw new Error('Shrubbery became dirty during the build; refusing to publish')
  }
  return Boolean(dirty || currentDirty)
}

function acquireBuildLock(lockPath) {
  mkdirSync(dirname(lockPath), { recursive: true })
  try {
    mkdirSync(lockPath)
  } catch (error) {
    if (error.code === 'EEXIST') {
      throw new Error(`Another build owns ${lockPath}. Wait for it; if a build crashed, inspect the owner before removing its stale lock.`)
    }
    throw error
  }
  const owner = JSON.stringify({ id: randomUUID(), pid: process.pid, commit: lock.commit })
  const ownerPath = resolve(lockPath, 'owner.json')
  try { writeFileSync(ownerPath, owner) } catch (error) {
    rmSync(lockPath, { recursive: true, force: true })
    throw error
  }
  const release = () => {
    // Avoid removing a replacement lock left by manual recovery.
    try {
      if (readFileSync(ownerPath, 'utf8') === owner) rmSync(lockPath, { recursive: true, force: true })
    } catch { /* A missing/replaced lock is not ours to clean up. */ }
  }
  process.once('exit', release)
  return () => { release(); process.removeListener('exit', release) }
}

function runPnpm(args) {
  const child = spawn('pnpm', ['--dir', shrubberyRoot, ...args], {
    stdio: 'inherit',
    env: process.env,
  })
  for (const signal of ['SIGINT', 'SIGTERM']) {
    process.once(signal, () => child.kill(signal))
  }
  return new Promise((resolveExit, reject) => {
    child.once('error', reject)
    child.once('exit', (code, signal) => {
      if (signal) reject(new Error(`pnpm terminated by ${signal}`))
      else if (code !== 0) reject(new Error(`pnpm exited with status ${code}`))
      else resolveExit()
    })
  })
}

if (mode === 'build') {
  const releaseDestination = acquireBuildLock(resolve(gardenRoot, 'src-tauri/target/.frontend-build-lock'))
  let releaseSource
  try {
    const sourceLock = gitHead
      ? resolve(shrubberyRoot, runGit(shrubberyRoot, ['rev-parse', '--git-path', 'garden-shrubbery-build.lock']).stdout.trim())
      : resolve(shrubberyRoot, '.garden-shrubbery-build-lock')
    releaseSource = acquireBuildLock(sourceLock)
    verifyBuildSource()
    await runPnpm(['install', '--frozen-lockfile'])
    await runPnpm(['--filter', lock.package, 'build'])
    const dirtyBuild = verifyBuildSource()

    const source = resolve(shrubberyRoot, lock.dist)
    const destination = resolve(gardenRoot, 'frontend/dist')
    if (!existsSync(resolve(source, 'index.html'))) {
      throw new Error(`Shrubbery build did not produce ${source}/index.html`)
    }
    rmSync(destination, { recursive: true, force: true })
    cpSync(source, destination, { recursive: true })
    writeFileSync(
      resolve(destination, 'shrubbery-build.json'),
      `${JSON.stringify({ repository: lock.repositoryUrl, commit: lock.commit, requestedRevision: lock.requestedRevision, resolvedFrom: lock.resolution, dirty: dirtyBuild }, null, 2)}\n`,
    )
    console.log(`Packaged ${lock.package}@${lock.commit.slice(0, 12)} for Garden`)
  } finally {
    releaseSource?.()
    releaseDestination()
  }
} else {
  if (!existsSync(resolve(shrubberyRoot, 'node_modules'))) await runPnpm(['install', '--frozen-lockfile'])
  const command = mode === 'dev' ? 'dev' : 'preview'
  await runPnpm([
    // pnpm forwards a literal standalone `--` to Vite here; pass the script
    // arguments directly so the pinned UI actually binds Garden's dev port.
    '--filter', lock.package, command,
    '--host', '127.0.0.1',
    '--port', '1420',
    '--strictPort',
  ])
}
