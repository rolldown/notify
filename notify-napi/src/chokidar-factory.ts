import { EventEmitter } from 'node:events'
import { lstatSync, statSync, type Stats } from 'node:fs'
import { relative, resolve, sep } from 'node:path'
import {
  createWatcherApi,
  type NotifyBindings,
  type Watcher,
  type Ignored,
  type WatcherEvent,
  type WatchMatcher,
} from './factory.js'

export type WatchEventName = 'add' | 'addDir' | 'change' | 'unlink' | 'unlinkDir'
export type { Ignored, WatchMatcher }

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
  ignored?: Ignored
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
const normalize = (value: string): string => value.replaceAll(sep, '/')

export interface FSWatcher extends EventEmitter {
  readonly options: WatchOptions
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
  FSWatcher: new (options?: WatchOptions) => FSWatcher
  watch: (paths: string | ReadonlyArray<string>, options?: WatchOptions) => FSWatcher
}

/** Create the Chokidar-compatible API using an addon's native bindings. */
export function createChokidarApi(binding: NotifyBindings): ChokidarApi {
  const { Watcher } = /* #__PURE__ */ createWatcherApi(binding)

  class FSWatcher extends EventEmitter {
    readonly options: WatchOptions
    closed = false
    private readonly cwd: string
    private readonly watcher: Watcher

    constructor(options: WatchOptions = {}) {
      super()
      this.options = options
      this.cwd = resolve(options.cwd ?? process.cwd())
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
      this.watcher.add(toArray(paths).map((path) => this.absolute(path)))
      return this
    }

    unwatch(paths: string | ReadonlyArray<string>): this {
      if (!this.closed) {
        this.watcher.remove(toArray(paths).map((path) => this.absolute(path)))
      }
      return this
    }

    /** Checks configured watch coverage; resolves paths against cwd. */
    isWatched(path: string): boolean {
      return !this.closed && this.watcher.isWatched(this.absolute(path))
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

    private absolute(path: string): string {
      return resolve(this.cwd, path)
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
      if (!event.path) return
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

  return { FSWatcher, watch }
}
