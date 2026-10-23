import { lstatSync, statSync, type Stats } from 'node:fs'
import { isAbsolute, relative, resolve, sep } from 'node:path'
import * as binding from '../dist/binding.mjs'

export type WatcherEventName =
  | 'add'
  | 'addDir'
  | 'change'
  | 'unlink'
  | 'unlinkDir'
  | 'error'
export type WatchMatcher =
  | string
  | RegExp
  | ((path: string, stats?: Stats) => boolean)
export type Ignored = WatchMatcher | WatchMatcher[]

export interface WatcherEvent {
  kind: WatcherEventName
  path?: string
  isDir?: boolean
  error?: string
}

export interface WatcherOptions {
  usePolling?: boolean
  pollInterval?: number
  followSymlinks?: boolean
  ignored?: Ignored
  cwd?: string
}

type CallbackMatcher = Exclude<WatchMatcher, string>

const toArray = <T>(value: T | ReadonlyArray<T>): T[] =>
  Array.isArray(value) ? [...value] : [value as T]
const normalize = (value: string): string => value.replaceAll(sep, '/')

function matches(
  matchers: CallbackMatcher[],
  path: string,
  stats: Stats | undefined,
): boolean {
  return matchers.some((matcher) => {
    if (matcher instanceof RegExp) return matcher.test(path)
    return matcher(path) || Boolean(stats && matcher(path, stats))
  })
}

function createIgnored(ignored: Ignored | undefined, options: WatcherOptions) {
  const values = ignored == null ? [] : toArray(ignored)
  const cwd = resolve(options.cwd ?? process.cwd())
  const stringMatchers = values.filter((value): value is string => typeof value === 'string')
  const callbackMatchers = values.filter(
    (value): value is CallbackMatcher => typeof value !== 'string',
  )
  const globs = stringMatchers.flatMap((value) => {
    const negated = value.startsWith('!')
    const pattern = negated ? value.slice(1) : value
    if (isAbsolute(pattern)) return [value]
    return [value, `${negated ? '!' : ''}${normalize(resolve(cwd, pattern))}`]
  })
  if (callbackMatchers.length === 0) return { globs, predicate: undefined }

  const stat = options.followSymlinks === false ? lstatSync : statSync
  const predicate = (path: string): boolean => {
    let stats: Stats | undefined
    try {
      stats = stat(path, { throwIfNoEntry: false })
    } catch {}
    const absolute = normalize(path)
    const local = normalize(relative(cwd, path))
    for (const candidate of local && local !== absolute ? [absolute, local] : [absolute]) {
      if (matches(callbackMatchers, candidate, stats)) return true
    }
    return false
  }
  return { globs, predicate }
}

export class Watcher {
  closed = false
  private readonly native: InstanceType<typeof binding.Watcher>
  private readonly ignored: ((path: string) => boolean) | undefined

  constructor(callback: (event: WatcherEvent) => void, options: WatcherOptions = {}) {
    if (typeof callback !== 'function') throw new TypeError('callback must be a function')
    const { ignored, cwd, ...nativeOptions } = options
    const matcher = createIgnored(ignored, { cwd, ...nativeOptions })
    this.ignored = matcher.predicate
    this.native = new binding.Watcher(
      (event) => {
        if (!this.closed && (!event.path || !this.isIgnored(event.path))) {
          callback(event as WatcherEvent)
        }
      },
      { ...nativeOptions, ignored: matcher.globs },
    )
  }

  add(paths: string | ReadonlyArray<string>): this {
    if (!this.closed) this.native.add(toArray(paths))
    return this
  }

  remove(paths: string | ReadonlyArray<string>): this {
    if (!this.closed) this.native.remove(toArray(paths))
    return this
  }

  /** Checks configured watch coverage, including ignores and exclusions. */
  isWatched(path: string): boolean {
    return !this.closed && this.native.isWatched(
      path,
      this.ignored ? (normalized) => this.isIgnored(normalized) : undefined,
    )
  }

  ref(): this {
    if (!this.closed) this.native.ref()
    return this
  }

  unref(): this {
    if (!this.closed) this.native.unref()
    return this
  }

  close(): void {
    if (!this.closed) {
      this.closed = true
      this.native.close()
    }
  }

  private isIgnored(path: string): boolean {
    if (!this.ignored) return false
    for (let candidate = path; ; candidate = resolve(candidate, '..')) {
      if (this.ignored(candidate)) return true
      if (resolve(candidate, '..') === candidate) return false
    }
  }
}

export function watch(
  paths: string | ReadonlyArray<string>,
  callback: (event: WatcherEvent) => void,
  options: WatcherOptions = {},
): Watcher {
  const watcher = new Watcher(callback, options)
  try {
    watcher.add(paths)
  } catch (error) {
    watcher.close()
    throw error
  }
  return watcher
}
