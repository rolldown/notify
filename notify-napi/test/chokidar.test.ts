import { once } from 'node:events'
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs'
import { spawnSync } from 'node:child_process'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { setImmediate as immediate, setTimeout as delay } from 'node:timers/promises'
import { expect, test } from 'vitest'
import { FSWatcher, watch, type WatchEventName } from '@rolldown/notify/chokidar'

function waitForEvent(
  watcher: FSWatcher,
  eventName: WatchEventName,
  predicate: (path: string) => boolean,
  timeout = 2000,
): Promise<void> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup()
      reject(new Error(`timed out waiting for ${eventName}`))
    }, timeout)
    const onEvent = (path: string) => {
      if (!predicate(path)) return
      cleanup()
      resolve()
    }
    const onError = (error: Error) => {
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
    expect(root in watched).toBeTruthy()
    expect(sourceDirectory in watched).toBeTruthy()
    expect(!(join(root, 'node_modules') in watched)).toBeTruthy()

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
    expect(ignoredEvent).toBe(false)
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
  expect(watcher.closed).toBe(true)
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
  const events: [WatchEventName, string][] = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    expect(events.some(([event, path]) => event === 'addDir' && path === root)).toBeTruthy()
    expect(events.some(([event, path]) => event === 'add' && path === file)).toBeTruthy()
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
  const added: string[] = []
  watcher.on('add', (path) => added.push(path))
  try {
    await once(watcher, 'ready')
    watcher.add(second)
    await delay(100)
    expect(!added.includes(file)).toBeTruthy()
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
  const events: [WatchEventName, string][] = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    expect(events.some(([event, path]) => event === 'add' && path === file)).toBeTruthy()
  } finally {
    await watcher.close()
  }

  const unwatched = watch(root)
  unwatched.unwatch(root)
  try {
    await once(unwatched, 'ready')
    expect(Object.keys(unwatched.getWatched())).toEqual([])
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
  const paths: string[] = []
  watcher.on('all', (_event, path) => paths.push(path))
  try {
    await once(watcher, 'ready')
    expect(receivedPathOnly).toBe(true)
    expect(receivedDirectoryStats).toBe(true)
    expect('.' in watcher.getWatched()).toBeTruthy()
    expect(!paths.some((path) => path.startsWith('ignored'))).toBeTruthy()
    expect(!paths.some((path) => path.startsWith('function-ignored'))).toBeTruthy()
    expect(!paths.some((path) => path.startsWith('regexp-ignored'))).toBeTruthy()
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
  const events: string[] = []
  watcher.on('all', (_event, path) => events.push(path))
  try {
    await once(watcher, 'ready')
    checkedIgnoredDirectory = false
    writeFileSync(ignoredFile, 'export default 1\n')
    const visible = waitForEvent(watcher, 'add', (path) => path === visibleFile)
    writeFileSync(visibleFile, 'export default 1\n')
    await visible
    await delay(100)
    expect(checkedIgnoredDirectory).toBe(true)
    expect(!events.includes(ignoredFile)).toBeTruthy()
    expect(!watcher.getWatched()[ignoredDirectory]).toBeTruthy()
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
  const paths: string[] = []
  watcher.on('all', (_event, path) => paths.push(path))
  try {
    await once(watcher, 'ready')
    expect(paths.includes('ignored/keep/kept.js')).toBeTruthy()
    expect(!paths.includes('ignored/drop/dropped.js')).toBeTruthy()
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
    expect(target in watcher.getWatched()).toBeTruthy()
    expect(!(link in watcher.getWatched())).toBeTruthy()
    const changed = waitForEvent(watcher, 'change', (path) => path === file)
    writeFileSync(file, "export default 'after'\n")
    await changed
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

// FSEvents follows symlinked watch roots.
test.skipIf(process.platform === 'darwin')('followSymlinks false does not traverse a symlinked directory', async () => {
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
  const events: [WatchEventName, string][] = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    expect(events.some(([event, path]) => event === 'add' && path === link)).toBeTruthy()
    expect(!events.some(([, path]) => path === file)).toBeTruthy()
    writeFileSync(file, "export default 'after'\n")
    await delay(150)
    expect(!events.some(([, path]) => path === file)).toBeTruthy()
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
  const events: [WatchEventName, string][] = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    watcher.unwatch(child)
    expect(!(child in watcher.getWatched())).toBeTruthy()
    events.length = 0
    watcher.add(child)
    expect(child in watcher.getWatched()).toBeTruthy()
    expect(watcher.getWatched()[child].includes('entry.js')).toBeTruthy()
    await immediate()
    expect(events.some(([event, path]) => event === 'addDir' && path === child)).toBeTruthy()
    expect(events.some(([event, path]) => event === 'add' && path === file)).toBeTruthy()
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('persistent false allows the process to exit', () => {
  const root = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-exit-'))
  try {
    for (const expression of [
      "watch('.', { persistent: false })",
      "watch('.', { persistent: false, ignored: () => false })",
    ]) {
      const script = `import(${JSON.stringify(new URL('../dist/chokidar.mjs', import.meta.url).href)}).then(({ watch }) => { ${expression} })`
      const result = spawnSync(process.execPath, ['-e', script], {
        cwd: root,
        timeout: 2000,
        encoding: 'utf8',
      })
      expect(result.signal, result.stderr).toBe(null)
      expect(result.status, result.stderr).toBe(0)
    }
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
})

test('watch closes the native watcher when add fails', () => {
  const script = `
    const binding = await import(${JSON.stringify(new URL('../dist/binding.mjs', import.meta.url).href)})
    binding.Watcher.prototype.add = () => { throw new Error('watch failed') }
    const { watch } = await import(${JSON.stringify(new URL('../dist/chokidar.mjs', import.meta.url).href)})
    try { watch('.') } catch {}
  `
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', script], {
    timeout: 2000,
    encoding: 'utf8',
  })
  expect(result.signal, result.stderr).toBe(null)
  expect(result.status, result.stderr).toBe(0)
})
