'use strict'

const assert = require('node:assert/strict')
const { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } = require('node:fs')
const { spawnSync } = require('node:child_process')
const { tmpdir } = require('node:os')
const { join } = require('node:path')
const { test } = require('node:test')

let Watcher
let watch
let chokidar
let binding

test.before(async () => {
  ;({ Watcher, watch } = await import('@rolldown/notify'))
  chokidar = await import('@rolldown/notify/chokidar')
  binding = await import('../dist/binding.mjs')
  if (process.env.NAPI_RS_FORCE_WASI === 'error') {
    assert.equal(binding.__napiBindingTarget, 'wasm32-wasi')
  }
})

test('root entry point exposes the callback API', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-callback-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, 'export default 1\n')
  const events = []
  const watcher = watch(file, (event) => events.push(event), {
    emitInitial: true,
    usePolling: true,
    pollInterval: 25,
  })
  try {
    await new Promise((resolve) => setImmediate(resolve))
    assert.ok(watcher instanceof Watcher)
    assert.ok(events.some((event) => event.kind === 'add' && event.path === file))
    assert.ok(watcher.entries().some((entry) => entry.path === file))
    assert.equal(watcher.remove(file), watcher)
    events.length = 0
    writeFileSync(file, 'export default 2\n')
    await new Promise((resolve) => setTimeout(resolve, 100))
    assert.ok(!events.some((event) => event.path === file))
    assert.deepEqual(watcher.entries(), [])
  } finally {
    watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('Chokidar entry point exports FSWatcher', () => {
  assert.equal(typeof chokidar.FSWatcher, 'function')
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
  const events = []
  const watcher = new binding.Watcher((event) => events.push(event), {
    ignored: [`${ignored}/**`, `!${keptDirectory}/**`],
    usePolling: true,
    pollInterval: 25,
  })
  try {
    const initial = watcher.add([root], true)
    assert.ok(!initial.some((event) => event.path === dropped))
    assert.ok(initial.some((event) => event.path === kept))
    assert.ok(!watcher.entries().some((entry) => entry.path === dropped))
    assert.ok(watcher.entries().some((entry) => entry.path === kept))
    await new Promise((resolve) => setTimeout(resolve, 50))
    writeFileSync(dropped, 'changed')
    writeFileSync(kept, 'changed')
    await new Promise((resolve) => setTimeout(resolve, 100))
    assert.ok(!events.some((event) => event.path === dropped))
    assert.ok(events.some((event) => event.path === kept))
  } finally {
    watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('watch closes the native watcher when add fails', () => {
  const modulePath = join(__dirname, '..', 'dist', 'index.mjs')
  const script = `
    const binding = await import(${JSON.stringify(join(__dirname, '..', 'dist', 'binding.mjs'))})
    binding.Watcher.prototype.add = () => { throw new Error('watch failed') }
    const { watch } = await import(${JSON.stringify(modulePath)})
    try { watch('.', () => {}) } catch {}
  `
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', script], {
    timeout: 2000,
    encoding: 'utf8',
  })
  assert.equal(result.signal, null, result.stderr)
  assert.equal(result.status, 0, result.stderr)
})
