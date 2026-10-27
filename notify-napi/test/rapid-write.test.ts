import { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { setTimeout as delay } from 'node:timers/promises'
import { test } from 'vitest'
import { Watcher } from '@rolldown/notify'

test('watcher reports the final state of rapid writes', async () => {
  const directory = mkdtempSync(join(realpathSync.native(tmpdir()), 'rolldown-notify-napi-'))
  const file = join(directory, 'entry.js')
  writeFileSync(file, "export default 'initial'\n")

  let expected: string | undefined
  let resolveExpected: (() => void) | undefined
  let active = true
  const watcher = new Watcher((event) => {
    if (!active || !expected || !['add', 'change'].includes(event.kind) || event.path !== file) {
      return
    }
    if (readFileSync(file, 'utf8').trim() === expected) {
      resolveExpected?.()
    }
  })
  try {
    watcher.add(file)
    await delay(100)
    for (let round = 0; round < 10; round += 1) {
      const nextEventFor = (value: string) => {
        expected = `export default '${value}'`
        return Promise.race([
          new Promise<void>((resolve) => {
            resolveExpected = resolve
          }),
          delay(500).then(() => {
            throw new Error(`timed out waiting for ${value}`)
          }),
        ])
      }

      const sawFirstWrite = nextEventFor(`${round}a`)
      writeFileSync(file, `export default '${round}a'\n`)
      await sawFirstWrite

      const sawFinalWrite = nextEventFor(`${round}b`)
      await delay(10)
      writeFileSync(file, `export default '${round}b'\n`)
      await sawFinalWrite
    }
  } finally {
    active = false
    watcher.close()
    rmSync(directory, { recursive: true, force: true })
  }
})
