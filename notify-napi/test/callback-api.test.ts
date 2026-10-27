import { mkdirSync, mkdtempSync, realpathSync, rmSync, statSync, utimesSync, writeFileSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { setImmediate as immediate, setTimeout as delay } from 'node:timers/promises'
import { beforeAll, expect, test } from 'vitest'
import { Watcher, watch, type WatcherEvent } from '@rolldown/notify'
import * as chokidar from '@rolldown/notify/chokidar'
import * as binding from '../dist/binding.mjs'

beforeAll(() => {
  if (process.env.NAPI_RS_FORCE_WASI === 'error') {
    expect('__napiBindingTarget' in binding && binding.__napiBindingTarget).toBe('wasm32-wasi')
  }
})

test('root entry point exposes the callback API', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-callback-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, 'export default 1\n')
  const mtime = statSync(file).mtimeMs / 1000
  const events: WatcherEvent[] = []
  const watcher = watch(file, (event) => events.push(event), {
    usePolling: true,
    pollInterval: 25,
  })
  try {
    await immediate()
    expect(watcher).toBeInstanceOf(Watcher)
    expect(watcher.isWatched(file)).toBe(true)
    expect(watcher.isWatched(join(file, 'child.js'))).toBe(false)
    expect(watcher.isWatched(root)).toBe(false)
    expect(events).toEqual([])
    writeFileSync(file, 'export default 2\n')
    // Polling needs a distinct mtime even on filesystems with coarse timestamps.
    utimesSync(file, mtime + 1, mtime + 1)
    await expect.poll(() => events, { timeout: 2000 }).toContainEqual(
      expect.objectContaining({ kind: 'change', path: file }),
    )
    expect(watcher.remove(file)).toBe(watcher)
    expect(watcher.isWatched(file)).toBe(false)
    events.length = 0
    writeFileSync(file, 'export default 3\n')
    utimesSync(file, mtime + 2, mtime + 2)
    await delay(100)
    expect(!events.some((event) => event.path === file)).toBeTruthy()
  } finally {
    watcher.close()
    expect(watcher.isWatched(file)).toBe(false)
    rmSync(root, { recursive: true, force: true })
  }
})

test('Chokidar entry point exports FSWatcher', () => {
  expect(typeof chokidar.FSWatcher).toBe('function')
})

test('isWatched resolves parent components without following symlinks', () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-coverage-'))
  const watcher = watch(root, () => {}, { usePolling: true, followSymlinks: false })
  try {
    expect(watcher.isWatched(join(root, 'future.js'))).toBe(true)
    expect(watcher.isWatched(`${root}/../outside`)).toBe(false)
    expect(watcher.isWatched(`${root}/child/../../outside`)).toBe(false)
  } finally {
    watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('native binding supports negated ignored globs', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-negated-glob-'))
  const ignored = join(root, 'ignored')
  const keptDirectory = join(ignored, 'keep')
  const droppedDirectory = join(ignored, 'drop')
  const dropped = join(droppedDirectory, 'entry.js')
  const kept = join(keptDirectory, 'entry.js')
  mkdirSync(keptDirectory, { recursive: true })
  mkdirSync(droppedDirectory)
  writeFileSync(dropped, '')
  writeFileSync(kept, '')
  const events: binding.WatcherEvent[] = []
  const watcher = new binding.Watcher((event) => events.push(event), {
    ignored: [`${ignored}/**`, `!${keptDirectory}/**`],
    usePolling: true,
    pollInterval: 25,
  })
  try {
    watcher.add([root])
    expect(watcher.isWatched(dropped)).toBe(false)
    expect(watcher.isWatched(kept)).toBe(true)
    await delay(50)
    writeFileSync(dropped, 'changed')
    writeFileSync(kept, 'changed')
    await delay(100)
    expect(!events.some((event) => event.path === dropped)).toBeTruthy()
    expect(events.some((event) => event.path === kept)).toBeTruthy()
  } finally {
    watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('watch closes the native watcher when add fails', () => {
  const modulePath = new URL('../dist/index.mjs', import.meta.url).href
  const script = `
    const binding = await import(${JSON.stringify(new URL('../dist/binding.mjs', import.meta.url).href)})
    binding.Watcher.prototype.add = () => { throw new Error('watch failed') }
    const { watch } = await import(${JSON.stringify(modulePath)})
    try { watch('.', () => {}) } catch {}
  `
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', script], {
    timeout: 2000,
    encoding: 'utf8',
  })
  expect(result.signal, result.stderr).toBe(null)
  expect(result.status, result.stderr).toBe(0)
})
