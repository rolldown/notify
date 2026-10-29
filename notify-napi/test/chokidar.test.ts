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
import { join, sep } from 'node:path'
import { setTimeout as delay } from 'node:timers/promises'
import { expect, test } from 'vitest'
import { FSWatcher, watch, type WatchEventName } from '@rolldown/notify/chokidar'

function waitForEvent(
  watcher: FSWatcher,
  eventNames: WatchEventName | WatchEventName[],
  predicate: (path: string) => boolean,
  timeout = 2000,
): Promise<void> {
  const names = Array.isArray(eventNames) ? eventNames : [eventNames]
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup()
      reject(new Error(`timed out waiting for ${names.join(' or ')}`))
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
      for (const name of names) watcher.off(name, onEvent)
      watcher.off('error', onError)
    }
    for (const name of names) watcher.on(name, onEvent)
    watcher.on('error', onError)
  })
}

test('watcher handles lifecycle and file events', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-vite-'))
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
    ignored: ['**/node_modules/**'],
    usePolling: true,
    pollInterval: 25,
  })
  try {
    await once(watcher, 'ready')
    expect(watcher.isWatched(root)).toBe(true)
    expect(watcher.isWatched(entry)).toBe(true)
    expect(watcher.isWatched(added)).toBe(true)
    expect(watcher.isWatched(ignored)).toBe(false)
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
  expect(watcher.isWatched(entry)).toBe(false)
})

test('watcher forwards polling options', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-poll-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, "export default 'before'\n")
  const watcher = watch(file, {
    usePolling: true,
    pollInterval: 25,
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

for (const usePolling of [false, true]) {
  test(`glob paths filter file events (polling: ${usePolling})`, async () => {
    const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-glob-'))
    const source = join(root, 'src')
    mkdirSync(source)
    mkdirSync(join(source, 'nested'))
    mkdirSync(join(source, 'ignored'))
    const entry = join(source, 'entry.js')
    const renamed = join(source, 'nested', 'renamed.ts')
    writeFileSync(entry, 'before')
    const watcher = watch(['src/**/*.{js,ts}', '!src/**/skip*'], {
      cwd: root,
      usePolling,
      pollInterval: 25,
      ignored: '**/ignored/**',
    })
    const events: [WatchEventName, string][] = []
    watcher.on('all', (event, path) => events.push([event, path]))
    try {
      await once(watcher, 'ready')
      expect(events).toEqual([])
      expect(watcher.isWatched('src/entry.js')).toBe(true)
      expect(watcher.isWatched('src/nested/future.ts')).toBe(true)
      expect(watcher.isWatched('src/.hidden.js')).toBe(true)
      expect(watcher.isWatched('src/other.css')).toBe(false)
      expect(watcher.isWatched('src/skip.js')).toBe(false)
      expect(watcher.isWatched('src/ignored/entry.js')).toBe(false)
      expect(watcher.isWatched('outside.js')).toBe(false)

      const changed = waitForEvent(watcher, ['add', 'change'], (path) => path === 'src/entry.js')
      writeFileSync(entry, 'after!')
      await changed
      const removed = usePolling
        ? waitForEvent(watcher, 'unlink', (path) => path === 'src/entry.js')
        : Promise.resolve()
      const added = waitForEvent(watcher, 'add', (path) => path === 'src/nested/renamed.ts')
      renameSync(entry, renamed)
      await Promise.all([removed, added])
      writeFileSync(join(source, 'other.css'), 'filtered')
      writeFileSync(join(source, 'skip.js'), 'filtered')
      writeFileSync(join(source, 'ignored', 'entry.js'), 'filtered')
      const hidden = waitForEvent(watcher, 'add', (path) => path === 'src/.hidden.js')
      writeFileSync(join(source, '.hidden.js'), 'visible')
      await hidden
      await delay(100)
      expect(events.every(([, path]) => /\.(js|ts)$/.test(path) &&
        !path.includes('skip') && !path.includes('ignored'))).toBe(true)
    } finally {
      await watcher.close()
      rmSync(root, { recursive: true, force: true })
    }
  })
}

test('glob syntax, absolute paths, and cwd with glob characters', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-[glob]-'))
  mkdirSync(join(root, 'src'))
  const watcher = watch(['src/{entry,other}[0-9].[jt]s', 'src/single?.js', 'src/{one/two,three}/*.js'], { cwd: root })
  try {
    await once(watcher, 'ready')
    expect(watcher.isWatched('src/entry1.js')).toBe(true)
    expect(watcher.isWatched('src/other2.ts')).toBe(true)
    expect(watcher.isWatched('src/entry12.js')).toBe(false)
    expect(watcher.isWatched('src/single1.js')).toBe(true)
    expect(watcher.isWatched('src/single12.js')).toBe(false)
    expect(watcher.isWatched('src/one/two/entry.js')).toBe(true)
    expect(watcher.isWatched('src/one/entry.js')).toBe(false)
    expect(watcher.isWatched('src/three/entry.js')).toBe(true)
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }

  const absoluteRoot = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-absolute-glob-'))
  const absolute = watch(join(absoluteRoot, '*.js'))
  try {
    await once(absolute, 'ready')
    expect(absolute.isWatched(join(absoluteRoot, 'entry.js'))).toBe(true)
    expect(absolute.isWatched(join(absoluteRoot, 'nested', 'entry.js'))).toBe(false)
  } finally {
    await absolute.close()
    rmSync(absoluteRoot, { recursive: true, force: true })
  }
})

for (const usePolling of [false, true]) {
  test(`glob unwatch preserves overlapping roots and supports re-add (polling: ${usePolling})`, async () => {
    const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-glob-unwatch-'))
    mkdirSync(join(root, 'src'))
    const watcher = watch(['**/*.js', 'src/*.{js,ts}'], { cwd: root, usePolling, pollInterval: 25 })
    try {
      await once(watcher, 'ready')
      watcher.unwatch('**/*.js')
      expect(watcher.isWatched('src/entry.js')).toBe(false)
      expect(watcher.isWatched('src/entry.ts')).toBe(true)
      expect(watcher.isWatched('elsewhere.ts')).toBe(false)
      let added = waitForEvent(watcher, 'add', (path) => path === 'src/entry.ts')
      writeFileSync(join(root, 'src', 'entry.ts'), 'one')
      await added
      watcher.add('**/*.js')
      expect(watcher.isWatched('src/entry.js')).toBe(true)
      watcher.unwatch('src')
      expect(watcher.isWatched('src/entry.js')).toBe(false)
      expect(watcher.isWatched('outside.js')).toBe(true)
      watcher.add('src/deep/**/*.js')
      expect(watcher.isWatched('src/entry.js')).toBe(true)
      watcher.add('src')
      watcher.unwatch('**/*.js')
      expect(watcher.isWatched('src/entry.js')).toBe(false)
      expect(watcher.isWatched('src/entry.css')).toBe(true)
      added = waitForEvent(watcher, 'add', (path) => path === 'src/entry.css')
      writeFileSync(join(root, 'src', 'entry.css'), 'visible')
      await added
      watcher.unwatch('src')
      expect(watcher.isWatched('src/entry.css')).toBe(false)
    } finally {
      await watcher.close()
      rmSync(root, { recursive: true, force: true })
    }
  })
}

test('glob parents created after ready are watched', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-missing-glob-'))
  const watcher = watch('future/nested/**/*.js', { cwd: root, usePolling: true, pollInterval: 25 })
  try {
    await once(watcher, 'ready')
    expect(watcher.isWatched('future/nested/entry.js')).toBe(true)
    expect(watcher.isWatched('outside.js')).toBe(false)
    mkdirSync(join(root, 'future', 'nested'), { recursive: true })
    const added = waitForEvent(watcher, 'add', (path) => path === 'future/nested/entry.js')
    writeFileSync(join(root, 'future', 'nested', 'entry.js'), 'visible')
    await added
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('disableGlobbing watches literal glob-like directories', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-literal-glob-'))
  mkdirSync(join(root, '[literal]'))
  mkdirSync(join(root, 'l'))
  const watcher = watch('[literal]', { cwd: root, disableGlobbing: true, usePolling: true, pollInterval: 25 })
  try {
    await once(watcher, 'ready')
    expect(watcher.isWatched('[literal]/entry.js')).toBe(true)
    expect(watcher.isWatched('l/entry.js')).toBe(false)
    const added = waitForEvent(watcher, 'add', (path) => path === '[literal]/entry.js')
    writeFileSync(join(root, '[literal]', 'entry.js'), 'visible')
    await added
    watcher.unwatch('[literal]')
    expect(watcher.isWatched('[literal]/entry.js')).toBe(false)
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

for (const usePolling of [false, true]) {
  test(`glob paths under symlinked roots match backend paths (polling: ${usePolling})`, async () => {
    const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-glob-link-'))
    const target = join(root, 'target')
    const link = join(root, 'link')
    mkdirSync(target)
    symlinkSync(target, link)
    const watcher = watch('link/**/*.js', { cwd: root, usePolling, pollInterval: 25 })
    try {
      await once(watcher, 'ready')
      expect(watcher.isWatched('link/entry.js')).toBe(true)
      expect(watcher.isWatched('target/entry.js')).toBe(true)
      expect(watcher.isWatched('target/entry.css')).toBe(false)
      const added = waitForEvent(watcher, 'add', (path) => path === 'target/entry.js')
      writeFileSync(join(target, 'entry.js'), 'visible')
      await added
      watcher.unwatch('target/entry.js')
      expect(watcher.isWatched('link/entry.js')).toBe(false)
      expect(watcher.isWatched('target/entry.js')).toBe(false)
      watcher.add('link/entry.js')
      expect(watcher.isWatched('link/entry.js')).toBe(true)
      expect(watcher.isWatched('target/entry.js')).toBe(true)
      watcher.unwatch('target')
      expect(watcher.isWatched('link/other.js')).toBe(false)
    } finally {
      await watcher.close()
      rmSync(root, { recursive: true, force: true })
    }
  })
}

test('polling reports changes using the default interval', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-poll-default-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, "export default 'before'\n")
  const watcher = watch(file, { usePolling: true })
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
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-rename-'))
  const before = join(root, 'before.js')
  const after = join(root, 'after.js')
  writeFileSync(before, 'export default 1\n')
  const watcher = watch(root, { usePolling: true, pollInterval: 25 })
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

test('watching existing files emits no initial events', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-initial-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, 'export default 1\n')
  const watcher = watch(root, { usePolling: true, pollInterval: 25 })
  const events: [WatchEventName, string][] = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    expect(events).toEqual([])
    expect(watcher.isWatched(file)).toBe(true)
    const changed = waitForEvent(watcher, 'change', (path) => path === file)
    writeFileSync(file, 'export default 2\n')
    await changed
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('adding paths after ready registers them without initial events', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-late-add-'))
  const first = join(root, 'first')
  const second = join(root, 'second')
  const file = join(second, 'entry.js')
  mkdirSync(first)
  mkdirSync(second)
  writeFileSync(file, 'export default 1\n')
  const watcher = watch(first, { usePolling: true, pollInterval: 25 })
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

test('direct FSWatcher chaining registers paths and unwatch is synchronous', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-direct-'))
  const file = join(root, 'entry.js')
  writeFileSync(file, 'export default 1\n')
  const watcher = new FSWatcher().add(file)
  const events: [WatchEventName, string][] = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    expect(events).toEqual([])
    expect(watcher.isWatched(file)).toBe(true)
    expect(watcher.isWatched(join(file, 'child.js'))).toBe(false)
  } finally {
    await watcher.close()
  }

  const unwatched = watch(root)
  const unwatchedEvents: string[] = []
  unwatched.on('all', (_event, path) => unwatchedEvents.push(path))
  unwatched.unwatch(root)
  try {
    await once(unwatched, 'ready')
    expect(unwatched.isWatched(file)).toBe(false)
    writeFileSync(file, 'export default 2\n')
    await delay(100)
    expect(unwatchedEvents).toEqual([])
  } finally {
    await unwatched.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('ignored matchers receive stats and honor cwd-relative globs', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-ignored-'))
  const ignoredDirectory = join(root, 'ignored')
  const functionDirectory = join(root, 'function-ignored')
  const functionPath = functionDirectory.replaceAll(sep, '/')
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
    ignored: [
      'ignored/**',
      /regexp-ignored/,
      (path, stats) => {
        if (path === functionPath && stats === undefined) {
          receivedPathOnly = true
        } else if (path === functionPath && stats?.isDirectory()) {
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
    expect(watcher.isWatched('.')).toBe(true)
    expect(watcher.isWatched('visible.js')).toBe(true)
    expect(watcher.isWatched('ignored/relative.js')).toBe(false)
    expect(watcher.isWatched('function-ignored/function.js')).toBe(false)
    expect(watcher.isWatched('regexp-ignored/regexp.js')).toBe(false)
    const visible = waitForEvent(watcher, 'add', (path) => path === 'visible.js')
    writeFileSync(join(ignoredDirectory, 'relative.js'), 'changed')
    writeFileSync(join(functionDirectory, 'function.js'), 'changed')
    writeFileSync(join(regexpDirectory, 'regexp.js'), 'changed')
    writeFileSync(join(root, 'visible.js'), 'visible')
    await visible
    await delay(100)
    expect(receivedPathOnly).toBe(true)
    expect(receivedDirectoryStats).toBe(true)
    expect(!paths.some((path) => path.startsWith('ignored'))).toBeTruthy()
    expect(!paths.some((path) => path.startsWith('function-ignored'))).toBeTruthy()
    expect(!paths.some((path) => path.startsWith('regexp-ignored'))).toBeTruthy()
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('function ignores filter descendants created after ready', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-runtime-ignore-'))
  const ignoredDirectory = join(root, 'ignored')
  const ignoredPath = ignoredDirectory.replaceAll(sep, '/')
  const ignoredFile = join(ignoredDirectory, 'entry.js')
  const visibleFile = join(root, 'visible.js')
  mkdirSync(ignoredDirectory)
  let checkedIgnoredDirectory = false
  const watcher = watch(root, {
    ignored(path, stats) {
      if (path === ignoredPath && stats?.isDirectory()) {
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
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test('negated globs re-include nested subtrees', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-negated-'))
  const kept = join(root, 'ignored', 'keep', 'kept.js')
  const dropped = join(root, 'ignored', 'drop', 'dropped.js')
  mkdirSync(join(root, 'ignored', 'keep'), { recursive: true })
  mkdirSync(join(root, 'ignored', 'drop'))
  writeFileSync(kept, '')
  writeFileSync(dropped, '')
  const watcher = watch('.', {
    cwd: root,
    ignored: ['ignored/**', '!ignored/keep/**'],
  })
  const paths: string[] = []
  watcher.on('all', (_event, path) => paths.push(path))
  try {
    await once(watcher, 'ready')
    const changed = waitForEvent(watcher, ['add', 'change'], (path) => path === 'ignored/keep/kept.js')
    writeFileSync(kept, 'kept change')
    writeFileSync(dropped, 'dropped change')
    await changed
    await delay(100)
    expect(paths.includes('ignored/keep/kept.js')).toBeTruthy()
    expect(!paths.includes('ignored/drop/dropped.js')).toBeTruthy()
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

// WalkDir cannot follow nested symlinks on WASI because same-file is unsupported.
test.skipIf(process.env.NAPI_RS_FORCE_WASI === 'error')('followed symlinks inside a root are covered by their alias', async () => {
  const directory = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-link-coverage-'))
  const root = join(directory, 'watched')
  const target = join(directory, 'target')
  const link = join(root, 'link')
  const internal = join(root, 'internal')
  const internalLink = join(root, 'internal-link')
  const file = join(link, 'entry.js')
  mkdirSync(root)
  mkdirSync(target)
  mkdirSync(internal)
  writeFileSync(join(target, 'entry.js'), 'before')
  writeFileSync(join(internal, 'entry.js'), 'internal')
  symlinkSync(target, link)
  symlinkSync(internal, internalLink)
  const watcher = watch(root, {
    usePolling: true,
    pollInterval: 25,
    ignored: ['**/internal-link/**'],
  })
  try {
    await once(watcher, 'ready')
    expect(watcher.isWatched(file)).toBe(true)
    expect(watcher.isWatched(join(link, 'future.js'))).toBe(true)
    expect(watcher.isWatched(join(target, 'entry.js'))).toBe(false)
    expect(watcher.isWatched(`${root}/../target/entry.js`)).toBe(false)
    expect(watcher.isWatched(join(internalLink, 'entry.js'))).toBe(false)
    expect(watcher.isWatched(join(internal, 'entry.js'))).toBe(true)
    const changed = waitForEvent(watcher, 'change', (path) => path === file)
    writeFileSync(file, 'after')
    await changed
  } finally {
    await watcher.close()
    rmSync(directory, { recursive: true, force: true })
  }
})

test('symlinked roots use canonical backend paths', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-symlink-'))
  const target = join(root, 'target')
  const link = join(root, 'link')
  const file = join(target, 'entry.js')
  mkdirSync(target)
  writeFileSync(file, "export default 'before'\n")
  symlinkSync(target, link)
  const watcher = watch(link)
  try {
    await once(watcher, 'ready')
    expect(watcher.isWatched(link)).toBe(true)
    expect(watcher.isWatched(target)).toBe(true)
    expect(watcher.isWatched(file)).toBe(true)
    const changed = waitForEvent(watcher, ['add', 'change'], (path) => path === file)
    writeFileSync(file, "export default 'after'\n")
    await changed
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

// FSEvents follows symlinked watch roots.
test.skipIf(process.platform !== 'darwin' || process.env.NAPI_RS_FORCE_WASI === 'error')('FSEvents reports canonical paths when followSymlinks is false', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-fsevents-paths-'))
  const target = join(root, 'target')
  const link = join(root, 'link')
  const file = join(target, 'entry.js')
  mkdirSync(target)
  writeFileSync(file, 'before')
  symlinkSync(target, link)
  const watcher = watch(link, { followSymlinks: false })
  try {
    await once(watcher, 'ready')
    expect(watcher.isWatched(file)).toBe(true)
    const changed = waitForEvent(watcher, ['add', 'change'], (path) => path === file)
    writeFileSync(file, 'after')
    await changed
  } finally {
    await watcher.close()
    rmSync(root, { recursive: true, force: true })
  }
})

test.skipIf(process.platform === 'darwin')('followSymlinks false does not traverse a symlinked directory', async () => {
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-no-follow-'))
  const target = join(root, 'target')
  const link = join(root, 'link')
  const file = join(target, 'entry.js')
  mkdirSync(target)
  writeFileSync(file, "export default 'before'\n")
  symlinkSync(target, link)
  const watcher = watch(link, {
    followSymlinks: false,
  })
  const events: [WatchEventName, string][] = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    expect(events).toEqual([])
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
  const root = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-rewatch-'))
  const child = join(root, 'child')
  const file = join(child, 'entry.js')
  mkdirSync(child)
  writeFileSync(file, "export default 'before'\n")
  const watcher = watch(root, { usePolling: true, pollInterval: 25 })
  const events: [WatchEventName, string][] = []
  watcher.on('all', (event, path) => events.push([event, path]))
  try {
    await once(watcher, 'ready')
    watcher.unwatch(child)
    expect(watcher.isWatched(file)).toBe(false)
    writeFileSync(file, 'export default 1\n')
    await delay(100)
    expect(events).toEqual([])
    watcher.add(child)
    expect(watcher.isWatched(file)).toBe(true)
    expect(events).toEqual([])
    const changed = waitForEvent(watcher, 'change', (path) => path === file)
    writeFileSync(file, 'export default 2\n')
    await changed
  } finally {
    await watcher.close()
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
