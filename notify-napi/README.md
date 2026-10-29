# `@rolldown/notify`

Node bindings for `rolldown-notify` with a callback API:

```js
import { watch } from '@rolldown/notify'

const watcher = watch(
  ['/project', '/shared/vite.config.js'],
  (event) => console.log(event.kind, event.path),
  {
    ignored: ['**/node_modules/**', '**/*.map', '!**/keep.map'],
  },
)

watcher.add('/another/root')
watcher.remove('/project')
watcher.close()
```

## Chokidar-like API

The separate `@rolldown/notify/chokidar` entry point adapts the callback API to Chokidar's EventEmitter interface:

```js
import chokidar from '@rolldown/notify/chokidar'

const watcher = chokidar.watch('/project')
watcher.on('change', (path) => console.log(path))
await watcher.close()
```

Watch paths are treated as literal paths rather than globs.

Existing files do not emit initial events, and a directory event does not synthesize events for its descendants. Removed paths whose type is unavailable from the backend emit `unlink`.

Use `isWatched(path)` to check configured watch coverage. A file root covers that file, and a directory root covers its descendants, including paths that do not exist yet. Ignored or unwatched paths return `false`, as does any query on a closed watcher. The check does not enumerate directories or guarantee OS event delivery. `getWatched()` is not supported.

`unwatch()` can exclude a descendant of a recursively watched root even when that descendant was not added separately. The parent remains watched by the native backend, but the adapter suppresses events below the excluded path. Adding the descendant again clears the exclusion. Adding a deeper path also clears the ancestor exclusion for the whole subtree; re-inclusion holes are not represented independently.

## `followSymlinks: false`

Symlink behavior follows the backend: the macOS FSEvents backend canonicalizes watch roots and does not support the `follow_symlinks` option. Polling uses WalkDir, which follows a symlink supplied as the root independently of its `follow_links` setting.
