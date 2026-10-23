import { defineConfig } from 'vitest/config'

export default defineConfig(({ mode }) => ({
  test: {
    include: ['test/*.test.ts'],
    pool: 'forks',
    env: mode === 'wasi' ? { NAPI_RS_FORCE_WASI: 'error' } : {},
  },
}))
