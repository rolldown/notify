'use strict'

const assert = require('node:assert/strict')
const { once } = require('node:events')
const {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} = require('node:fs')
const { spawnSync } = require('node:child_process')
const { tmpdir } = require('node:os')
const { join } = require('node:path')
const { test } = require('node:test')

let FSWatcher
let watch

test.before(async () => {
  ;({ FSWatcher, watch } = await import('@rolldown/notify/chokidar'))
})

const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds))

function waitForEvent(watcher, eventName, predicate, timeout = 2000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup()
      reject(new Error(`timed out waiting for ${eventName}`))
    }, timeout)
    const onEvent = (...args) => {
      if (!predicate(...args)) return
      cleanup()
      resolve(args)
    }
    const onError = (error) => {
      cleanup()
      reject(error)
    }
    const cleanup = () => {
      clearTimeout(timer)
      watcher.off(eventName, onEvent)
      watcher.off('error', onError)
    }
    watcher.on(eventName, onEvent)
    watcher.on('error', onError)
  })
}

test('watcher handles lifecycle and file events', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-vite-'))
  const sourceDirectory = join(root, 'src')
  const dependencyDirectory = join(root, 'node_modules', 'dependency')
  const entry = join(sourceDirectory, 'entry.js')
  const added = join(sourceDirectory, 'added.js')
  const ignored = join(dependencyDirectory, 'index.js')
  mkdirSync(sourceDirectory)
  mkdirSync(dependencyDirectory, { recursive: true })
  writeFileSync(entry, "export default 'initial'\n")
  writeFileSync(ignored, "export default 'ignored'\n")

  const watcher = watch(root, {
    ignoreInitial: true,
    ignored: ['**/node_modules/**'],
  })
  try {
    await once(watcher, 'ready')
    const watched = watcher.getWatched()
    assert.ok(root in watched)
    assert.ok(sourceDirectory in watched)
    assert.ok(!(join(root, 'node_modules') in watched))

    const firstWrite = waitForEvent(
      watcher,
      'change',
      (path) => path === entry && readFileSync(entry, 'utf8').includes("'a'"),
    )
    writeFileSync(entry, "export default 'a'\n")
    await firstWrite

    const finalWrite = waitForEvent(
      watcher,
      'change',
      (path) => path === entry && readFileSync(entry, 'utf8').includes("'b'"),
    )
    await delay(10)
    writeFileSync(entry, "export default 'b'\n")
    await finalWrite

    const fileAdded = waitForEvent(watcher, 'add', (path) => path === added)
    writeFileSync(added, 'export default 1\n')
    await fileAdded

    const fileRemoved = waitForEvent(watcher, 'unlink', (path) => path === added)
    rmSync(added)
    await fileRemoved

    let ignoredEvent = false
    watcher.on('all', (_event, path) => {
      if (path === ignored) ignoredEvent = true
    })
    writeFileSync(ignored, "export default 'still ignored'\n")
    await delay(150)
    assert.equal(ignoredEvent, false)
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
  assert.equal(watcher.closed, true)
})

test('watcher forwards polling options', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-poll-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, "export default 'before'\n")
  const watcher = watch(file, {
    ignoreInitial: true,
    usePolling: true,
    interval: 25,
  })
  try {
    await once(watcher, 'ready')
    const changed = waitForEvent(watcher, 'change', (path) => path === file)
    writeFileSync(file, "export default 'after!'\n")
    await changed
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('polling reports changes using the default interval', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-poll-default-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, "export default 'before'\n")
  const watcher = watch(file, { ignoreInitial: true, usePolling: true })
  try {
    await once(watcher, 'ready')
    const changed = waitForEvent(watcher, 'change', (path) => path === file, 1500)
    writeFileSync(file, "export default 'after'\n")
    await changed
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('renames emit unlink and add events', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-rename-'))
  const before = join(root, 'before.js')
  const after = join(root, 'after.js')
  writeFileSync(before, 'export default 1\n')
  const watcher = watch(root, { ignoreInitial: true })
  try {
    await once(watcher, 'ready')
    const removed = waitForEvent(watcher, 'unlink', (path) => path === before)
    const added = waitForEvent(watcher, 'add', (path) => path === after)
    renameSync(before, after)
    await Promise.all([removed, added])
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('watcher emits initial events after listeners attach', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-initial-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, 'export default 1\n')
  const watcher = watch(root, { ignoreInitial: false })
  const events = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    assert.ok(events.some(([event, path]) => event === 'addDir' && path === root))
    assert.ok(events.some(([event, path]) => event === 'add' && path === file))
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('ignoreInitial applies to paths added after ready', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-late-add-'))
  const first = join(root, 'first')
  const second = join(root, 'second')
  const file = join(second, 'entry.js')
  mkdirSync(first)
  mkdirSync(second)
  writeFileSync(file, 'export default 1\n')
  const watcher = watch(first, { ignoreInitial: true, usePolling: true, interval: 25 })
  const added = []
  watcher.on('add', (path) => added.push(path))
  try {
    await once(watcher, 'ready')
    watcher.add(second)
    await delay(100)
    assert.ok(!added.includes(file))
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('direct FSWatcher chaining emits initial events and unwatch is synchronous', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-direct-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, 'export default 1\n')
  const watcher = new FSWatcher().add(root)
  const events = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    assert.ok(events.some(([event, path]) => event === 'add' && path === file))
  } finally {
    await watcher.close()
  }

  const unwatched = watch(root)
  unwatched.unwatch(root)
  try {
    await once(unwatched, 'ready')
    assert.deepEqual(Object.keys(unwatched.getWatched()), [])
  } finally {
    await unwatched.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('ignored matchers receive stats and honor cwd-relative globs', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-ignored-'))
  const ignoredDirectory = join(root, 'ignored')
  const functionDirectory = join(root, 'function-ignored')
  const regexpDirectory = join(root, 'regexp-ignored')
  mkdirSync(ignoredDirectory)
  mkdirSync(functionDirectory)
  mkdirSync(regexpDirectory)
  writeFileSync(join(ignoredDirectory, 'relative.js'), '')
  writeFileSync(join(functionDirectory, 'function.js'), '')
  writeFileSync(join(regexpDirectory, 'regexp.js'), '')
  let receivedPathOnly = false
  let receivedDirectoryStats = false
  const watcher = watch('.', {
    cwd: root,
    ignoreInitial: false,
    ignored: [
      'ignored/**',
      /regexp-ignored/,
      (path, stats) => {
        if (path === functionDirectory && stats === undefined) {
          receivedPathOnly = true
        } else if (path === functionDirectory && stats?.isDirectory()) {
          receivedDirectoryStats = true
          return true
        }
        return false
      },
    ],
  })
  const paths = []
  watcher.on('all', (_event, path) => paths.push(path))
  try {
    await once(watcher, 'ready')
    assert.equal(receivedPathOnly, true)
    assert.equal(receivedDirectoryStats, true)
    assert.ok('.' in watcher.getWatched())
    assert.ok(!paths.some((path) => path.startsWith('ignored')))
    assert.ok(!paths.some((path) => path.startsWith('function-ignored')))
    assert.ok(!paths.some((path) => path.startsWith('regexp-ignored')))
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('function ignores prune descendants created after ready', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-runtime-ignore-'))
  const ignoredDirectory = join(root, 'ignored')
  const ignoredFile = join(ignoredDirectory, 'entry.js')
  const visibleFile = join(root, 'visible.js')
  mkdirSync(ignoredDirectory)
  let checkedIgnoredDirectory = false
  const watcher = watch(root, {
    ignoreInitial: true,
    ignored(path, stats) {
      if (path === ignoredDirectory && stats?.isDirectory()) {
        checkedIgnoredDirectory = true
        return true
      }
      return false
    },
  })
  const events = []
  watcher.on('all', (_event, path) => events.push(path))
  try {
    await once(watcher, 'ready')
    checkedIgnoredDirectory = false
    writeFileSync(ignoredFile, 'export default 1\n')
    const visible = waitForEvent(watcher, 'add', (path) => path === visibleFile)
    writeFileSync(visibleFile, 'export default 1\n')
    await visible
    await delay(100)
    assert.equal(checkedIgnoredDirectory, true)
    assert.ok(!events.includes(ignoredFile))
    assert.ok(!watcher.getWatched()[ignoredDirectory])
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('negated globs re-include nested subtrees', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-negated-'))
  const kept = join(root, 'ignored', 'keep', 'kept.js')
  const dropped = join(root, 'ignored', 'drop', 'dropped.js')
  mkdirSync(join(root, 'ignored', 'keep'), { recursive: true })
  mkdirSync(join(root, 'ignored', 'drop'))
  writeFileSync(kept, '')
  writeFileSync(dropped, '')
  const watcher = watch('.', {
    cwd: root,
    ignoreInitial: false,
    ignored: ['ignored/**', '!ignored/keep/**'],
  })
  const paths = []
  watcher.on('all', (_event, path) => paths.push(path))
  try {
    await once(watcher, 'ready')
    assert.ok(paths.includes('ignored/keep/kept.js'))
    assert.ok(!paths.includes('ignored/drop/dropped.js'))
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('symlinked roots share the canonical watched snapshot', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-symlink-'))
  const target = join(root, 'target')
  const link = join(root, 'link')
  const file = join(target, 'entry.js')
  mkdirSync(target)
  writeFileSync(file, "export default 'before'\n")
  symlinkSync(target, link)
  const watcher = watch(link, { ignoreInitial: true })
  try {
    await once(watcher, 'ready')
    assert.ok(target in watcher.getWatched())
    assert.ok(!(link in watcher.getWatched()))
    const changed = waitForEvent(watcher, 'change', (path) => path === file)
    writeFileSync(file, "export default 'after'\n")
    await changed
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('followSymlinks false does not traverse a symlinked directory', {
  skip: process.platform === 'darwin' && 'FSEvents follows symlinked watch roots',
}, async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-no-follow-'))
  const target = join(root, 'target')
  const link = join(root, 'link')
  const file = join(target, 'entry.js')
  mkdirSync(target)
  writeFileSync(file, "export default 'before'\n")
  symlinkSync(target, link)
  const watcher = watch(link, {
    followSymlinks: false,
    ignoreInitial: false,
  })
  const events = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    assert.ok(events.some(([event, path]) => event === 'add' && path === link))
    assert.ok(!events.some(([, path]) => path === file))
    writeFileSync(file, "export default 'after'\n")
    await delay(150)
    assert.ok(!events.some(([, path]) => path === file))
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('an implicitly watched child can be unwatched and re-added', async () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-rewatch-'))
  const child = join(root, 'child')
  const file = join(child, 'entry.js')
  mkdirSync(child)
  writeFileSync(file, "export default 'before'\n")
  const watcher = watch(root, { ignoreInitial: false, usePolling: true, interval: 25 })
  const events = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    watcher.unwatch(child)
    assert.ok(!(child in watcher.getWatched()))
    events.length = 0
    watcher.add(child)
    assert.ok(child in watcher.getWatched())
    assert.ok(watcher.getWatched()[child].includes('entry.js'))
    await new Promise((resolve) => setImmediate(resolve))
    assert.ok(events.some(([event, path]) => event === 'addDir' && path === child))
    assert.ok(events.some(([event, path]) => event === 'add' && path === file))
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('persistent false and unref allow the process to exit', () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-exit-'))
  try {
    for (const expression of [
      "watch('.', { persistent: false })",
      "watch('.', { persistent: false, ignored: () => false })",
      "watch('.').unref()",
      "watch('.', { ignored: () => false }).unref()",
    ]) {
      const script = `import(${JSON.stringify(join(__dirname, '..', 'dist', 'chokidar.mjs'))}).then(({ watch }) => { ${expression} })`
      const result = spawnSync(process.execPath, ['-e', script], {
        cwd: root,
        timeout: 2000,
        encoding: 'utf8',
      })
      assert.equal(result.signal, null, result.stderr)
      assert.equal(result.status, 0, result.stderr)
    }
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
})

test('watch closes the native watcher when add fails', () => {
  const script = `
    const binding = await import(${JSON.stringify(join(__dirname, '..', 'dist', 'binding.mjs'))})
    binding.Watcher.prototype.add = () => { throw new Error('watch failed') }
    const { watch } = await import(${JSON.stringify(join(__dirname, '..', 'dist', 'chokidar.mjs'))})
    try { watch('.') } catch {}
  `
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', script], {
    timeout: 2000,
    encoding: 'utf8',
  })
  assert.equal(result.signal, null, result.stderr)
  assert.equal(result.status, 0, result.stderr)
})
