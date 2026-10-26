import { spawnSync } from 'node:child_process'
import { mkdtempSync, realpathSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { expect, test } from 'vitest'

function runWatcherScript(entry: 'index' | 'chokidar', ignored: boolean, body: string) {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-ref-'))
  try {
    writeFileSync(join(root, 'entry.js'), 'before')
    const script = `
      import assert from 'node:assert/strict'
      import { writeFileSync } from 'node:fs'
      import { join } from 'node:path'
      import { watch } from ${JSON.stringify(new URL(`../dist/${entry}.mjs`, import.meta.url).href)}

      const file = join(process.cwd(), 'entry.js')
      const options = {
        usePolling: true,
        ${entry === 'index' ? 'pollInterval' : 'interval'}: 20,
        ignored: ${ignored ? '() => false' : 'undefined'},
      }
      const watcher = ${entry === 'index'
        ? "watch(process.cwd(), (event) => { if (event.kind === 'change' && event.path === file) onChange() }, options)"
        : "watch(process.cwd(), options).on('change', (path) => { if (path === file) onChange() })"}
      ${body}
    `
    const result = spawnSync(process.execPath, ['--input-type=module', '-e', script], {
      cwd: root,
      timeout: 5000,
      encoding: 'utf8',
    })
    expect(result.error, result.error?.message).toBeUndefined()
    expect(result.signal, result.stderr).toBe(null)
    expect(result.status, result.stderr).toBe(0)
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
}

for (const entry of ['index', 'chokidar'] as const) {
  for (const ignored of [false, true]) {
    const name = `${entry}${ignored ? ' with an ignore callback' : ''}`

    test(`${name}: unref allows the process to exit`, () => {
      runWatcherScript(entry, ignored, `
        function onChange() {}
        assert.equal(watcher.unref(), watcher)
        assert.equal(watcher.unref(), watcher)
      `)
    })

    test(`${name}: ref restores event-loop liveness until unref`, () => {
      runWatcherScript(entry, ignored, `
        let changed = false
        function onChange() {
          changed = true
          assert.equal(watcher.unref(), watcher)
          assert.equal(watcher.unref(), watcher)
        }
        process.once('beforeExit', () => {
          assert.ok(changed, 'ref() must keep the watcher alive to receive the change')
        })
        assert.equal(watcher.unref(), watcher)
        assert.equal(watcher.ref(), watcher)
        assert.equal(watcher.ref(), watcher)
        setTimeout(() => writeFileSync(file, 'after'), 50).unref()
      `)
    })
  }
}
