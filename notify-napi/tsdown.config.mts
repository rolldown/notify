import { defineConfig } from 'tsdown'

export default defineConfig({
  entry: [
    'src/index.ts',
    'src/chokidar.ts',
    'src/factory.ts',
    'src/chokidar-factory.ts',
  ],
  format: 'esm',
  outDir: 'dist',
  clean: false,
  dts: true,
  fixedExtension: true,
  target: 'node20',
  deps: {
    neverBundle: ['../dist/binding.mjs'],
  },
})
