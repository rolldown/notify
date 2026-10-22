# `@rolldown/notify`

Node bindings for `rolldown-notify` with a callback API:

```js
import { watch } from '@rolldown/notify'

const watcher = watch(
  ['/project', '/shared/vite.config.js'],
  (event) => console.log(event.kind, event.path),
  {
    emitInitial: false,
    ignored: ['**/node_modules/**', '**/*.map', '!**/keep.map'],
  },
)

watcher.add('/another/root')
watcher.remove('/project')
watcher.close()
```

## Chokidar compatibility

The separate `@rolldown/notify/chokidar` entry point adapts the callback API to Chokidar's EventEmitter interface:

```js
import chokidar from '@rolldown/notify/chokidar'

const watcher = chokidar.watch('/project', { ignoreInitial: true })
watcher.on('change', (path) => console.log(path))
await watcher.close()
```

Watch paths are treated as literal paths rather than globs.

`unwatch()` can exclude a descendant of a recursively watched root even when that descendant was not added separately. The parent remains watched by the native backend, but the adapter suppresses events below the excluded path and removes those entries from `getWatched()`. Adding the descendant again clears the exclusion. Adding a deeper path also clears the ancestor exclusion for the whole subtree; re-inclusion holes are not represented independently.

## Known bugs with `followSymlinks: false`

`followSymlinks: false` uses the default platform backend unless polling is explicitly enabled with `usePolling: true`. WASM always uses polling.

- The scanner still traverses a symlink supplied as a watch root because WalkDir follows root symlinks independently of its `follow_links` setting. Initial events and `getWatched()` can include descendants under the symlink's path. This also affects polling.
- The macOS FSEvents backend canonicalizes watch roots and does not support the `follow_symlinks` option. A symlink root watches its target, so target changes can emit events under the resolved target path. Removing or replacing the symlink itself may not be detected.
- Native events are not restricted to the requested roots or checked for symlink ancestors before scanning. Events under a resolved target path or beneath a symlink can therefore add entries and emit events outside the intended no-follow snapshot.
