#!/usr/bin/env node

/**
 * Resolve Garden's authoritative Shrubbery checkout coordinates.
 *
 * GitHub Actions cannot interpolate a step output into an action `uses:` ref.
 * Workflows therefore resolve this lock first and pass its outputs to a normal
 * actions/checkout step. The frontend adapter remains the final provenance and
 * dirty-tree enforcement gate.
 */

import { appendFileSync, existsSync, readFileSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const scriptPath = fileURLToPath(import.meta.url)
const gardenRoot = resolve(dirname(scriptPath), '..')

export function parseShrubberyLock(source, sourceName = 'shrubbery-ui.lock.json') {
  let lock
  try {
    lock = JSON.parse(source)
  } catch (error) {
    throw new Error(`${sourceName} is not valid JSON: ${error.message}`)
  }

  if (!lock || typeof lock !== 'object' || Array.isArray(lock)) {
    throw new Error(`${sourceName} must contain a JSON object`)
  }
  if (typeof lock.repository !== 'string') {
    throw new Error(`${sourceName} repository must be a GitHub HTTPS URL`)
  }

  let repositoryUrl
  try {
    repositoryUrl = new URL(lock.repository)
  } catch {
    throw new Error(`${sourceName} repository must be a GitHub HTTPS URL`)
  }
  const pathParts = repositoryUrl.pathname.replace(/\.git$/, '').split('/').filter(Boolean)
  if (
    repositoryUrl.protocol !== 'https:' ||
    repositoryUrl.hostname !== 'github.com' ||
    repositoryUrl.port !== '' ||
    repositoryUrl.username !== '' ||
    repositoryUrl.password !== '' ||
    repositoryUrl.search !== '' ||
    repositoryUrl.hash !== '' ||
    !pathParts.every(part => /^[A-Za-z0-9_.-]+$/.test(part)) ||
    pathParts.length !== 2
  ) {
    throw new Error(`${sourceName} repository must be a GitHub HTTPS URL`)
  }
  if (lock.commit !== 'main' && (typeof lock.commit !== 'string' || !/^[0-9a-f]{40}$/.test(lock.commit))) {
    throw new Error(`${sourceName} commit must be main or a lowercase 40-character SHA`)
  }
  if (
    typeof lock.package !== 'string'
    || lock.package.length === 0
    || /[\r\n]/.test(lock.package)
  ) {
    throw new Error(`${sourceName} package must be a non-empty string`)
  }
  if (
    typeof lock.dist !== 'string'
    || lock.dist.length === 0
    || /[\r\n\\]/.test(lock.dist)
    || lock.dist.startsWith('/')
    || lock.dist.split('/').some(part => part === '' || part === '.' || part === '..')
  ) {
    throw new Error(`${sourceName} dist must be a safe relative path`)
  }

  return Object.freeze({
    repository: pathParts.join('/'),
    repositoryUrl: lock.repository,
    commit: lock.commit,
    package: lock.package,
    dist: lock.dist,
  })
}

export function readShrubberyLock(lockPath = resolve(gardenRoot, 'shrubbery-ui.lock.json')) {
  return parseShrubberyLock(readFileSync(lockPath, 'utf8'), lockPath)
}

const isCommit = value => typeof value === 'string' && /^[0-9a-f]{40}$/.test(value)

export function gitAuthenticationArgs({ env = process.env, run = spawnSync } = {}) {
  const authenticated = run('gh', ['auth', 'status', '--hostname', 'github.com'], {
    stdio: 'ignore', env,
  }).status === 0
  return authenticated ? ['-c', 'credential.helper=!gh auth git-credential'] : []
}

/** Resolve mutable policy ONCE. All subsequent checkout/build steps use commit. */
export function resolveShrubberyLock(lock, {
  localRoot,
  env = process.env,
  run = spawnSync,
  remote = false,
} = {}) {
  const gitEnv = { ...env, GIT_TERMINAL_PROMPT: '0' }
  const candidate = localRoot === undefined ? env.SHRUBBERY_DIR || resolve(gardenRoot, '..', 'mycl-shrubbery') : localRoot
  const localRepository = candidate && existsSync(resolve(candidate, '.git'))
    ? resolve(candidate) : null
  const result = (commit, resolution, sourceRoot = null) => Object.freeze({
    ...lock, commit, requestedRevision: lock.commit, resolution, sourceRoot,
  })
  const frozen = env.SHRUBBERY_RESOLVED_REVISION
  if (frozen !== undefined) {
    if (!isCommit(frozen)) throw new Error('SHRUBBERY_RESOLVED_REVISION must be an exact lowercase 40-character SHA')
    if (lock.commit !== 'main' && frozen !== lock.commit) {
      throw new Error('Resolved Shrubbery revision conflicts with the explicit pin')
    }
    return result(frozen, 'resolved', localRepository)
  }
  if (lock.commit !== 'main') {
    if (!isCommit(lock.commit)) throw new Error('Shrubbery revision must be main or an exact SHA')
    return result(lock.commit, 'pinned', localRepository)
  }
  const ci = env.CI && !['false', '0'].includes(String(env.CI).toLowerCase())
  if (!remote && !ci && localRepository) {
    const head = run('git', ['-C', localRepository, 'rev-parse', '--verify', 'refs/heads/main^{commit}'], {
      encoding: 'utf8', env: gitEnv,
    })
    const commit = head.stdout?.trim()
    if (head.status !== 0 || !isCommit(commit)) {
      throw new Error(`Shrubbery checkout ${localRepository} has no readable local main; fetch/create main explicitly`)
    }
    return result(commit, 'local-main', localRepository)
  }
  const auth = gitAuthenticationArgs({ env: gitEnv, run })
  const response = run('git', [...auth, 'ls-remote', '--exit-code', '--refs', lock.repositoryUrl, 'refs/heads/main'], {
    encoding: 'utf8', env: gitEnv,
  })
  const lines = response.stdout?.trim().split(/\r?\n/).filter(Boolean) ?? []
  const parts = lines.length === 1 ? lines[0].split(/\s+/) : []
  if (response.status !== 0 || !isCommit(parts[0]) || parts[1] !== 'refs/heads/main') {
    throw new Error(`Unable to resolve ${lock.repository} main; authenticate GitHub and check connectivity. No cached revision was substituted.`)
  }
  return result(parts[0], 'remote-main')
}

export function githubOutput(lock) {
  if (!isCommit(lock.commit)) throw new Error('Resolve Shrubbery main to an exact SHA before emitting checkout outputs')
  return [
    `repository=${lock.repository}`,
    `repository_url=${lock.repositoryUrl}`,
    `commit=${lock.commit}`,
    `package=${lock.package}`,
    `dist=${lock.dist}`,
    '',
  ].join('\n')
}

function main() {
  const args = process.argv.slice(2)
  const outputIndex = args.indexOf('--github-output')
  const outputPath = outputIndex >= 0 ? args[outputIndex + 1] : null
  if (outputIndex >= 0 && !outputPath) {
    throw new Error('Usage: node scripts/resolve-shrubbery-lock.mjs [--github-output PATH]')
  }
  const lock = resolveShrubberyLock(readShrubberyLock(), { remote: args.includes('--remote') })
  if (outputPath) appendFileSync(outputPath, githubOutput(lock))
  else process.stdout.write(`${JSON.stringify(lock, null, 2)}\n`)
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  try {
    main()
  } catch (error) {
    console.error(error instanceof Error ? error.message : error)
    process.exitCode = 1
  }
}
