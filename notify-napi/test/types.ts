import notify, {
  Watcher,
  watch,
  type WatchMatcher,
  type WatcherEvent,
} from '@rolldown/notify'
import chokidar, {
  FSWatcher,
  type WatchOptions,
} from '@rolldown/notify/chokidar'

const ignored: WatchMatcher[] = ['**/node_modules/**', '!**/node_modules/keep/**']
const callback = (event: WatcherEvent): void => void event
const watcher: Watcher = watch('.', callback, { ignored })
watcher.close()

const options: WatchOptions = { ignored, ignoreInitial: true }
const fsWatcher: FSWatcher = chokidar.watch('.', options)
void fsWatcher.close()

void notify.Watcher
void FSWatcher
