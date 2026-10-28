import * as binding from '../dist/binding.mjs'
import { createWatcherApi, type WatcherApi, type Watcher as WatcherInstance } from './factory.js'

const api = /* #__PURE__ */ createWatcherApi(binding)
export const Watcher: WatcherApi['Watcher'] = /* #__PURE__ */ (() => api.Watcher)()
export const watch: WatcherApi['watch'] = /* #__PURE__ */ (() => api.watch)()
export type Watcher = WatcherInstance
export type {
  Ignored,
  WatcherEvent,
  WatcherEventName,
  WatcherOptions,
  WatchMatcher,
} from './factory.js'
