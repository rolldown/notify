import { EventEmitter } from 'node:events'
import { lstatSync, statSync, type Stats } from 'node:fs'
import { basename, dirname, relative, resolve, sep } from 'node:path'
import {
  Watcher,
  type WatcherEvent,
  type WatchMatcher,
} from './index.js'

export type WatchEventName = 'add' | 'addDir' | 'change' | 'unlink' | 'unlinkDir'
export type { WatchMatcher }

export interface NativeWatchEvent {
  kind: string
  path?: string
  isDir?: boolean
  error?: string
}

export interface AwaitWriteFinishOptions {
  stabilityThreshold?: number
  pollInterval?: number
}

export interface WatchOptions {
  ignored?: WatchMatcher | WatchMatcher[]
  ignoreInitial?: boolean
  followSymlinks?: boolean
  cwd?: string
  usePolling?: boolean
  interval?: number
  ignorePermissionErrors?: boolean
  persistent?: boolean
  disableGlobbing?: boolean
  useFsEvents?: boolean
  alwaysStat?: boolean
  depth?: number
  binaryInterval?: number
  atomic?: boolean | number
  awaitWriteFinish?: AwaitWriteFinishOptions | boolean
}

const toArray = <T>(value: T | ReadonlyArray<T>): T[] =>
  Array.isArray(value) ? [...value] : [value as T]
const normalize = (value: string): string => value.split(sep).join('/')

export class FSWatcher extends EventEmitter {
  readonly options: WatchOptions
  closed = false
  private readonly cwd: string | undefined
  private readonly watcher: Watcher

  constructor(options: WatchOptions = {}) {
    super()
    this.options = options
    this.cwd = options.cwd ? resolve(options.cwd) : undefined
    this.watcher = new Watcher((event) => this.handle(event), {
      usePolling: options.usePolling,
      pollInterval: options.interval ?? 100,
      followSymlinks: options.followSymlinks,
      ignored: options.ignored,
      cwd: this.cwd,
    })
    if (options.persistent === false) this.watcher.unref()

    setImmediate(() => {
      if (!this.closed) this.emit('ready')
    })
  }

  add(paths: string | ReadonlyArray<string>): this {
    if (this.closed) return this
    const initial = this.watcher.add(toArray(paths).map((path) => this.absolute(path)), {
      emitInitial: this.options.ignoreInitial !== true,
    })
    queueMicrotask(() => {
      if (!this.closed) initial.forEach((event) => this.handle(event))
    })
    return this
  }

  unwatch(paths: string | ReadonlyArray<string>): this {
    if (!this.closed) {
      this.watcher.remove(toArray(paths).map((path) => this.absolute(path)))
    }
    return this
  }

  getWatched(): Record<string, string[]> {
    const entries = this.watcher.entries().sort((left, right) => left.path.length - right.path.length)
    const directories = new Set(entries.filter((entry) => entry.isDir).map((entry) => entry.path))
    const watched: Record<string, string[]> = Object.create(null)
    for (const entry of entries) {
      if (entry.isDir) watched[this.display(entry.path)] ??= []
      const parent = dirname(entry.path)
      if (directories.has(parent)) {
        const children = (watched[this.display(parent)] ??= [])
        const name = basename(entry.path)
        if (!children.includes(name)) children.push(name)
      }
    }
    return watched
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
    this.removeAllListeners()
  }

  on(event: 'add' | 'addDir' | 'change', listener: (path: string, stats?: Stats) => void): this
  on(event: 'all', listener: (event: WatchEventName, path: string, stats?: Stats) => void): this
  on(event: 'error', listener: (error: Error) => void): this
  on(event: 'raw', listener: (event: string, path: string, details: NativeWatchEvent) => void): this
  on(event: 'ready', listener: () => void): this
  on(event: 'unlink' | 'unlinkDir', listener: (path: string) => void): this
  on(event: string, listener: (...args: any[]) => void): this
  on(event: string, listener: (...args: any[]) => void): this {
    return super.on(event, listener)
  }

  private absolute(path: string): string {
    return resolve(this.cwd ?? process.cwd(), path)
  }

  private display(path: string): string {
    if (!this.cwd) return path
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
    if (!event.path) return
    const stats = event.kind === 'unlink' || event.kind === 'unlinkDir'
      ? undefined
      : this.stats(event.path)
    const path = this.display(event.path)
    this.emit(event.kind, path, stats)
    this.emit('all', event.kind, path, stats)
  }
}

export function watch(
  paths: string | ReadonlyArray<string>,
  options?: WatchOptions,
): FSWatcher {
  const watcher = new FSWatcher(options)
  try {
    return watcher.add(paths)
  } catch (error) {
    void watcher.close()
    throw error
  }
}

const notify = { FSWatcher, watch }

export default notify
