const readonly = names => Object.fromEntries(names.split(' ').map(name => [name, 'readonly']));
export default [
  { ignores: ['node_modules/**', 'acceptance-output/**'] },
  {
    files: ['src/**/*.{mjs,cjs}', 'scripts/**/*.mjs', 'test/**/*.{mjs,cjs}'],
    languageOptions: {
      ecmaVersion: 'latest',
      globals: readonly('process Buffer console URL Response setTimeout clearTimeout setInterval clearInterval setImmediate structuredClone crypto fetch AbortController TextDecoder'),
    },
    rules: {
      'no-undef': 'error', 'no-unreachable': 'error', 'no-dupe-args': 'error',
      'no-dupe-keys': 'error', 'no-constant-condition': ['error', { checkLoops: false }],
      'no-unsafe-finally': 'error', 'valid-typeof': 'error',
    },
  },
  { files: ['**/*.cjs'], languageOptions: { sourceType: 'commonjs', globals: readonly('require module exports __dirname __filename') } },
  { files: ['src/renderer.mjs', 'src/presentation.mjs', 'scripts/*.mjs'], languageOptions: { globals: readonly('window document localStorage matchMedia requestAnimationFrame cancelAnimationFrame ResizeObserver HTMLElement HTMLTextAreaElement HTMLInputElement HTMLButtonElement KeyboardEvent Event getComputedStyle navigator location performance innerWidth innerHeight') } },
];
