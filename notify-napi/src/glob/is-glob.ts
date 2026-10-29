/*!
 * Based on is-glob v4.0.3
 * License: https://github.com/micromatch/is-glob/blob/v4.0.3/LICENSE
 */

const chars: Record<string, string> = { '{': '}', '(': ')', '[': ']' }
export default function isGlob(str: string): boolean {
  if (typeof str !== 'string' || str === '') {
    return false
  }

  if (str[0] === '!') {
    return true
  }
  let index = 0
  let closeSquareIndex = -2
  let closeCurlyIndex = -2
  let backSlashIndex = -2
  while (index < str.length) {
    if (str[index] === '*' || str[index] === '?') {
      return true
    }

    if (closeSquareIndex !== -1 && str[index] === '[' && str[index + 1] !== ']') {
      if (closeSquareIndex < index) {
        closeSquareIndex = str.indexOf(']', index)
      }
      if (closeSquareIndex > index) {
        if (backSlashIndex === -1 || backSlashIndex > closeSquareIndex) {
          return true
        }
        backSlashIndex = str.indexOf('\\', index)
        if (backSlashIndex === -1 || backSlashIndex > closeSquareIndex) {
          return true
        }
      }
    }

    if (closeCurlyIndex !== -1 && str[index] === '{' && str[index + 1] !== '}') {
      closeCurlyIndex = str.indexOf('}', index)
      if (closeCurlyIndex > index) {
        backSlashIndex = str.indexOf('\\', index)
        if (backSlashIndex === -1 || backSlashIndex > closeCurlyIndex) {
          return true
        }
      }
    }

    if (str[index] === '\\') {
      const open = str[index + 1]
      index += 2
      const close = chars[open]

      if (close) {
        const n = str.indexOf(close, index)
        if (n !== -1) {
          index = n + 1
        }
      }

      if (str[index] === '!') {
        return true
      }
    } else {
      index++
    }
  }
  return false
}
