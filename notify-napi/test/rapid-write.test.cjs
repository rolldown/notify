'use strict'

const { test } = require('node:test')
const { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } = require('node:fs')
const { tmpdir } = require('node:os')
const { join } = require('node:path')

let Watcher
let watcher

const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds))
const directory = mkdtempSync(join(realpathSync(tmpdir()), 'rolldown-notify-napi-'))
const file = join(directory, 'entry.js')
writeFileSync(file, "export default 'initial'\n")

let expected
let resolveExpected
let active = true
test.before(async () => {
  ;({ Watcher } = await import('@rolldown/notify'))
  watcher = new Watcher((event) => {
    if (!active || !expected || !['add', 'change'].includes(event.kind) || event.path !== file) {
      return
    }
    if (readFileSync(file, 'utf8').trim() === expected) {
      resolveExpected()
    }
  })
})

test('watcher reports the final state of rapid writes', async () => {
  try {
    watcher.add(file)
    await delay(100)
    for (let round = 0; round < 10; round += 1) {
      const nextEventFor = (value) => {
        expected = `export default '${value}'`
        return Promise.race([
          new Promise((resolve) => {
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
