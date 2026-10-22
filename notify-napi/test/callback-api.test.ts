import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from 'node:fs'
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
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-callback-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, 'export default 1\n')
  const events: WatcherEvent[] = []
  const watcher = watch(file, (event) => events.push(event), {
    emitInitial: true,
    usePolling: true,
    pollInterval: 25,
  })
  try {
    await immediate()
    expect(watcher).toBeInstanceOf(Watcher)
    expect(events.some((event) => event.kind === 'add' && event.path === file)).toBeTruthy()
    expect(watcher.entries().some((entry) => entry.path === file)).toBeTruthy()
    expect(watcher.remove(file)).toBe(watcher)
    events.length = 0
    writeFileSync(file, 'export default 2\n')
    await delay(100)
    expect(!events.some((event) => event.path === file)).toBeTruthy()
    expect(watcher.entries()).toEqual([])
  } finally {
    watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('Chokidar entry point exports FSWatcher', () => {
  expect(typeof chokidar.FSWatcher).toBe('function')
})

test('native binding supports negated ignored globs', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-negated-glob-'))
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
    const initial = watcher.add([root], true)
    expect(!initial.some((event) => event.path === dropped)).toBeTruthy()
    expect(initial.some((event) => event.path === kept)).toBeTruthy()
    expect(!watcher.entries().some((entry) => entry.path === dropped)).toBeTruthy()
    expect(watcher.entries().some((entry) => entry.path === kept)).toBeTruthy()
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
