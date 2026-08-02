// Bundles entry.jsx and glide's stylesheet into src/ui/vendor/.
//
//   cd tools/grid && npm install && npm run build
//
// You only need this to *change* the grid. Building or running alkyon does not:
// the output is committed, the same way CodeMirror and xterm are.

import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import { stat } from 'node:fs/promises';

const vendor = (name) =>
  fileURLToPath(new URL(`../../src/ui/vendor/${name}`, import.meta.url));

await build({
  entryPoints: ['entry.jsx'],
  bundle: true,
  minify: true,
  format: 'iife',
  target: 'es2020',
  // React reads this to drop its development warnings and checks; without it the
  // bundle is both larger and markedly slower.
  define: { 'process.env.NODE_ENV': '"production"' },
  legalComments: 'none',
  outfile: vendor('glide-data-grid.min.js'),
  logLevel: 'info',
});

// The stylesheet has to go through the bundler too.
//
// glide's published `dist/index.css` is nothing but a list of `@import`s
// pointing back inside the package, so copying that one file out gives you a
// stylesheet with no rules in it at all — and a grid whose container is
// `height: var(--wmyidgi-1)` with no declaration to read it, which lays out at
// zero pixels high and renders nothing. Bundling flattens the imports.
await build({
  entryPoints: ['node_modules/@glideapps/glide-data-grid/dist/index.css'],
  bundle: true,
  minify: true,
  outfile: vendor('glide-data-grid.css'),
  logLevel: 'info',
});

for (const name of ['glide-data-grid.min.js', 'glide-data-grid.css']) {
  const { size } = await stat(vendor(name));
  console.log(`${name}: ${(size / 1024).toFixed(0)} KB`);
}
