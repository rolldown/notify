import * as binding from '../dist/binding.mjs'
import {
  createChokidarApi,
  type ChokidarApi,
  type FSWatcher as FSWatcherInstance,
} from './chokidar-factory.js'

const chokidar: ChokidarApi = /* #__PURE__ */ createChokidarApi(binding)
export const FSWatcher: ChokidarApi['FSWatcher'] = /* #__PURE__ */ (() => chokidar.FSWatcher)()
export const watch: ChokidarApi['watch'] = /* #__PURE__ */ (() => chokidar.watch)()
export type FSWatcher = FSWatcherInstance
export type {
  AwaitWriteFinishOptions,
  Ignored,
  NativeWatchEvent,
  WatchEventName,
  WatchMatcher,
  WatchOptions,
} from './chokidar-factory.js'

export default chokidar
