import { EventEmitter } from 'node:events'
import { lstatSync, realpathSync, statSync, type Stats } from 'node:fs'
import { dirname, isAbsolute, relative, resolve, sep } from 'node:path'
import picomatch from 'picomatch'
import globParent from './glob/glob-parent.js'
import isGlob from './glob/is-glob.js'
import {
  createWatcherApi,
  type NotifyBindings,
  type Watcher,
  type Ignored,
  type WatcherEvent,
  type WatchMatcher,
  type WatcherOptions as BaseWatcherOptions,
} from './factory.js'

export type WatchEventName = 'add' | 'addDir' | 'change' | 'unlink' | 'unlinkDir'
export type { Ignored, WatchMatcher }

export interface WatcherOptions extends BaseWatcherOptions {
  ignorePermissionErrors?: boolean
  disableGlobbing?: boolean
}

export interface NativeWatchEvent {
  kind: string
  path?: string
  isDir?: boolean
  error?: string
}

const toArray = <T>(value: T | ReadonlyArray<T>): T[] =>
  Array.isArray(value) ? [...value] : [value as T]
const normalize = (value: string): string => value.replaceAll(sep, '/')
const contains = (root: string, path: string): boolean => {
  const local = relative(root, path)
  return local === '' || (local !== '..' && !local.startsWith(`..${sep}`) && !isAbsolute(local))
}

interface WatchPath {
  key: string
  base: string
  canonicalBase: string
  root: string
  glob?: (path: string) => boolean
  recursive: boolean
}

function matches(path: string, spec: WatchPath, descendants = false): boolean {
  for (const base of [spec.base, spec.canonicalBase]) {
    if (!contains(base, path)) continue
    const local = normalize(relative(base, path))
    if (!spec.glob) return spec.recursive || descendants || local === ''
    if (spec.glob(local)) return true
    if (descendants) {
      for (let parent = dirname(local); parent !== '.'; parent = dirname(parent)) {
        if (spec.glob(normalize(parent))) return true
      }
    }
  }
  return false
}

export interface FSWatcher extends EventEmitter {
  readonly options: WatcherOptions
  closed: boolean
  add(paths: string | ReadonlyArray<string>): this
  unwatch(paths: string | ReadonlyArray<string>): this
  isWatched(path: string): boolean
  ref(): this
  unref(): this
  close(): Promise<void>
  on(event: 'add' | 'addDir' | 'change', listener: (path: string, stats?: Stats) => void): this
  on(event: 'all', listener: (event: WatchEventName, path: string, stats?: Stats) => void): this
  on(event: 'error', listener: (error: Error) => void): this
  on(event: 'raw', listener: (event: string, path: string, details: NativeWatchEvent) => void): this
  on(event: 'ready', listener: () => void): this
  on(event: 'unlink' | 'unlinkDir', listener: (path: string) => void): this
  on(event: string, listener: (...args: any[]) => void): this
}

export interface ChokidarApi {
  FSWatcher: new (options?: WatcherOptions) => FSWatcher
  watch: (paths: string | ReadonlyArray<string>, options?: WatcherOptions) => FSWatcher
}

/** Create the Chokidar-compatible API using an addon's native bindings. */
export function createChokidarApi(binding: NotifyBindings): ChokidarApi {
  const { Watcher } = /* #__PURE__ */ createWatcherApi(binding)

  class FSWatcher extends EventEmitter {
    readonly options: WatcherOptions
    closed = false
    private readonly cwd: string
    private readonly watcher: Watcher
    private readonly paths = new Map<string, WatchPath>()
    private readonly excluded = new Map<string, WatchPath>()
    private readonly roots = new Set<string>()

    constructor(options: WatcherOptions = {}) {
      super()
      this.options = options
      this.cwd = resolve(options.cwd ?? process.cwd())
      this.watcher = new Watcher((event) => this.handle(event), {
        usePolling: options.usePolling,
        pollInterval: options.pollInterval,
        followSymlinks: options.followSymlinks,
        ignored: options.ignored,
        cwd: this.cwd,
      })

      setImmediate(() => {
        if (!this.closed) this.emit('ready')
      })
    }

    add(paths: string | ReadonlyArray<string>): this {
      if (this.closed) return this
      const specs = toArray(paths).map((path) => {
        const negative = path.startsWith('!')
        return { negative, spec: this.watchPath(negative ? path.slice(1) : path) }
      })
      const previous = new Map(this.paths)
      for (const { negative, spec } of specs) {
        if (!negative) this.paths.set(spec.key, spec)
      }
      try {
        this.syncRoots()
      } catch (error) {
        this.paths.clear()
        for (const [key, spec] of previous) this.paths.set(key, spec)
        try {
          this.syncRoots()
        } catch (restoreError) {
          throw new AggregateError([error, restoreError], 'Could not restore previous watch roots')
        }
        throw error
      }
      for (const { negative, spec } of specs) {
        if (negative) {
          this.excluded.set(spec.key, spec)
          continue
        }
        for (const [key, exclusion] of this.excluded) {
          if (key === spec.key || (!exclusion.glob && (
            contains(exclusion.base, spec.base) || contains(exclusion.canonicalBase, spec.canonicalBase)
          ))) {
            this.excluded.delete(key)
          }
        }
      }
      return this
    }

    unwatch(paths: string | ReadonlyArray<string>): this {
      if (!this.closed) {
        for (const path of toArray(paths)) {
          const spec = this.watchPath(path)
          this.excluded.set(spec.key, spec)
          for (const [key, watched] of this.paths) {
            if (key === spec.key || (!spec.glob && (
              contains(spec.base, watched.base) || contains(spec.canonicalBase, watched.canonicalBase)
            ))) {
              this.paths.delete(key)
            }
          }
        }
        this.syncRoots()
      }
      return this
    }

    /** Checks configured watch coverage; resolves paths against cwd. */
    isWatched(path: string): boolean {
      const absolute = this.absolute(path)
      return !this.closed && this.includes(absolute) && this.watcher.isWatched(absolute)
    }

    ref(): this {
      this.watcher.ref()
      return this
    }

    unref(): this {
      this.watcher.unref()
      return this
    }

    async close(): Promise<void> {
      if (this.closed) return
      this.closed = true
      this.watcher.close()
      this.paths.clear()
      this.excluded.clear()
      this.roots.clear()
      this.removeAllListeners()
    }

    private absolute(path: string): string {
      return resolve(this.cwd, path)
    }

    private watchPath(path: string): WatchPath {
      // Detect globs before resolving so glob characters in cwd remain literal.
      const pattern = normalize(path)
      const hasGlob = !this.options.disableGlobbing && isGlob(pattern)
      const parent = hasGlob ? globParent(pattern) : path
      const base = this.absolute(parent)
      // Parentheses and pipes stay literal when extglobs are disabled.
      const glob = hasGlob
        ? picomatch(
          normalize(relative(parent, pattern)).replace(/\\.|[()|]/g, (part) =>
            part.length === 1 ? `\\${part}` : part,
          ),
          { dot: true, noextglob: true, nonegate: true },
        )
        : undefined
      let root = base
      let canonicalBase = base
      // Missing glob parents are observed through their nearest existing ancestor.
      for (let ancestor = base; ; ancestor = dirname(ancestor)) {
        try {
          const canonical = realpathSync.native(ancestor)
          canonicalBase = resolve(canonical, relative(ancestor, base))
          if (glob) root = ancestor
          if (this.options.followSymlinks !== false ||
            (process.platform === 'darwin' && !this.options.usePolling &&
              process.env.NAPI_RS_FORCE_WASI !== 'error')) {
            root = glob ? canonical : canonicalBase
          }
          break
        } catch (error) {
          const code = (error as NodeJS.ErrnoException).code
          if ((code !== 'ENOENT' && code !== 'ENOTDIR') || dirname(ancestor) === ancestor) break
        }
      }
      return {
        key: this.absolute(path),
        base,
        canonicalBase,
        root,
        glob,
        recursive: !glob && statSync(base, { throwIfNoEntry: false })?.isDirectory() === true,
      }
    }

    private syncRoots(): void {
      const candidates = [...new Set([...this.paths.values()].map((spec) => spec.root))]
      const roots = candidates.filter((root) => !candidates.some(
        (other) => other !== root && contains(other, root),
      ))
      const removed: string[] = []
      for (const root of this.roots) {
        if (!roots.includes(root)) {
          this.watcher.remove(root)
          this.roots.delete(root)
          removed.push(root)
        }
      }
      for (const root of roots) {
        if (!this.roots.has(root)) {
          this.watcher.add(root)
          this.roots.add(root)
        }
      }
      // Native remove() leaves an exclusion. Clear it when a broader root
      // replaces the removed root; add() reuses that broader native watch.
      for (const root of removed) {
        if (roots.some((parent) => contains(parent, root))) this.watcher.add(root)
      }
    }

    private includes(path: string): boolean {
      for (const spec of this.paths.values()) {
        if (spec.base !== spec.canonicalBase && contains(spec.base, path)) {
          path = resolve(spec.canonicalBase, relative(spec.base, path))
          break
        }
      }
      for (const spec of this.excluded.values()) {
        if (matches(path, spec, true)) return false
      }
      for (const spec of this.paths.values()) {
        if (matches(path, spec)) return true
      }
      return false
    }

    private display(path: string): string {
      if (!this.options.cwd) return path
      return normalize(relative(this.cwd, path)) || '.'
    }

    private stats(path: string): Stats | undefined {
      try {
        const stat = this.options.followSymlinks === false ? lstatSync : statSync
        return stat(path, { throwIfNoEntry: false })
      } catch (error) {
        if (!this.ignoreError(error)) this.reportError(error)
      }
    }

    private ignoreError(error: unknown): boolean {
      const code = error instanceof Error && 'code' in error ? error.code : undefined
      const message = error instanceof Error ? error.message : String(error)
      return this.options.ignorePermissionErrors === true && (
        code === 'EACCES' || code === 'EPERM' ||
        /EACCES|EPERM|permission denied|operation not permitted/i.test(message)
      )
    }

    private reportError(error: unknown): void {
      queueMicrotask(() => {
        if (!this.closed) this.emit('error', error)
      })
    }

    private handle(event: WatcherEvent): void {
      if (this.closed) return
      if (event.kind === 'error') {
        const error = new Error(event.error)
        if (event.path) Object.assign(error, { path: event.path })
        if (!this.ignoreError(error)) this.emit('error', error)
        return
      }
      if (!event.path || !this.includes(event.path)) return
      const stats = event.kind === 'unlink' || event.kind === 'unlinkDir'
        ? undefined
        : this.stats(event.path)
      const path = this.display(event.path)
      this.emit(event.kind, path, stats)
      this.emit('all', event.kind, path, stats)
    }
  }

  function watch(
    paths: string | ReadonlyArray<string>,
    options?: WatcherOptions,
  ): FSWatcher {
    const watcher = new FSWatcher(options)
    try {
      return watcher.add(paths)
    } catch (error) {
      void watcher.close()
      throw error
    }
  }

  return { FSWatcher, watch }
}
