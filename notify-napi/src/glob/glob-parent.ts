/*!
 * Based on glob-parent v5.1.2
 * License: https://github.com/gulpjs/glob-parent/blob/v5.1.2/LICENSE
 */

import { posix } from 'node:path'
import isGlob from './is-glob.js'

const slash = '/'
const enclosure = /[\{\[].*[\}\]]$/
const globby = /(^|[^\\])([\{\[]|\([^\)]+$)/
const escaped = /\\([\!\*\?\|\[\]\(\)\{\}])/g

export default function globParent(str: string): string {
  // special case for strings ending in enclosure containing path separator
  if (enclosure.test(str)) {
    str += slash
  }

  // preserves full path in case of trailing path separator
  str += 'a'

  // remove path parts that are globby
  do {
    str = posix.dirname(str)
  } while (isGlob(str) || globby.test(str))

  // remove escape chars and return result
  return str.replace(escaped, '$1')
}
