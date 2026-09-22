import { build } from 'esbuild';
import { chmodSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
const root = fileURLToPath(new URL('./', import.meta.url));
await build({ absWorkingDir: root, entryPoints: ['src/cli.ts'], outfile: 'dist/ppr-tool.mjs', bundle: true, platform: 'node', format: 'esm', target: 'node22', banner: { js: '#!/usr/bin/env node' } });
await build({ absWorkingDir: root, entryPoints: ['src/manifest.ts'], outfile: 'dist/manifest.mjs', bundle: true, platform: 'node', format: 'esm', target: 'node22' });
chmodSync(new URL('./dist/ppr-tool.mjs', import.meta.url), 0o755);
