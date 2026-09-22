import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, readFile, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { gzipSync } from 'node:zlib';
import { zipSync, strToU8 } from 'fflate';
import { fileURLToPath } from 'node:url';
import { prepare, hash, packageMetadata } from '../dist/manifest.mjs';

const cli = fileURLToPath(new URL('../dist/ppr-tool.mjs', import.meta.url));
function tar(files) {
  const parts = [];
  for (const [name, text] of Object.entries(files)) {
    const data = Buffer.from(text), header = Buffer.alloc(512);
    header.write(name); header.write('0000644\0', 100); header.write(data.length.toString(8).padStart(11, '0') + '\0', 124);
    header.write('0', 156); header.write('ustar\0', 257); header.fill(32, 148, 156);
    header.write(header.reduce((a, b) => a + b, 0).toString(8).padStart(6, '0') + '\0 ', 148);
    parts.push(header, data, Buffer.alloc((512 - data.length % 512) % 512));
  }
  return gzipSync(Buffer.concat([...parts, Buffer.alloc(1024)]));
}
async function fixture(t) {
  const dir = await mkdtemp(path.join(tmpdir(), 'ppr-tool-test-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const metadata = { name: '@example/demo-sources', version: '1.2.3' };
  await writeFile(path.join(dir, 'pkg.tgz'), tar({ 'package/package.json': JSON.stringify(metadata) }));
  const manifest = { schemaVersion: 1, product: 'demo', version: '1.2.3', variant: 'sources', commit: 'a'.repeat(40), packages: [{ format: 'npm', ...metadata, file: 'pkg.tgz' }] };
  const file = path.join(dir, 'release.json');
  await writeFile(file, JSON.stringify(manifest));
  return { dir, file, manifest };
}
async function invoke(args, cwd, env = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [cli, ...args], { cwd, env: { ...process.env, PPR_TOKEN: 'test-secret', PPR_REGISTRY: '', GITHUB_ACTIONS: '', GITHUB_ENV: '', GITHUB_OUTPUT: '', ...env } });
    let output = '';
    child.stdout.on('data', data => output += data); child.stderr.on('data', data => output += data);
    child.on('error', reject); child.on('exit', code => resolve({ code, output }));
  });
}

test('archive metadata for all four formats', () => {
  assert.equal(packageMetadata('npm', tar({ 'package/package.json': '{"name":"demo","version":"1.2.3"}' })).name, 'demo');
  assert.equal(packageMetadata('composer', Buffer.from(zipSync({ 'composer.json': strToU8('{"name":"org/demo","version":"1.2.3"}') }))).name, 'org/demo');
  assert.equal(packageMetadata('nuget', Buffer.from(zipSync({ 'Demo.nuspec': strToU8('<package><metadata><id>Demo</id><version>1.2.3</version></metadata></package>') }))).name, 'Demo');
  assert.equal(packageMetadata('cargo', tar({ 'demo-1.2.3/Cargo.toml': '[package]\nname="demo"\nversion="1.2.3"\n' })).name, 'demo');
  assert.throws(() => packageMetadata('npm', tar({ '../package/package.json': '{}' })), /Invalid TAR/);
  assert.throws(() => packageMetadata('composer', Buffer.from(zipSync({ '../composer.json': strToU8('{}') }))), /Unsafe/);
});

test('manifest rejects missing fields, duplicate identities, traversal and mismatched artifacts', async t => {
  const { file, dir, manifest } = await fixture(t);
  const original = await prepare(file);
  assert.equal(original.release.packages[0].sha256, hash(await readFile(path.join(dir, 'pkg.tgz'))));
  for (const mutation of [
    m => delete m.product,
    m => m.packages.push(m.packages[0]),
    m => m.packages[0].file = '../pkg.tgz',
    m => m.packages[0].name = 'different',
    m => m.packages = [null]
  ]) {
    const value = structuredClone(manifest); mutation(value); await writeFile(file, JSON.stringify(value));
    await assert.rejects(prepare(file));
  }
  await writeFile(file, JSON.stringify(manifest));
  await writeFile(path.join(dir, 'pkg.tgz'), tar({ 'package/package.json': JSON.stringify({ name: manifest.packages[0].name, version: '1.2.3', private: true }) }));
  await assert.rejects(prepare(file), /Private workspace/);
});

test('stage retries and resumes; verify gates commit and detects changed artifacts', async t => {
  const { file, dir } = await fixture(t);
  let saved, digest, committed = false, puts = 0;
  const uploaded = new Set();
  const server = createServer(async (req, res) => {
    assert.equal(req.headers.authorization, 'Bearer test-secret');
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    const body = Buffer.concat(chunks);
    res.setHeader('content-type', 'application/json');
    if (req.method === 'POST' && req.url.endsWith('/releases')) {
      saved = JSON.parse(body); digest = hash(JSON.stringify(saved));
      assert.equal(req.headers['idempotency-key'], digest);
      assert.equal(saved.packages[0].file, undefined);
      res.end(JSON.stringify({ id: 'release-1', manifestSha256: digest, uploaded: [...uploaded] }));
    } else if (req.method === 'PUT') {
      puts++;
      assert.equal(hash(body), saved.packages[0].sha256);
      uploaded.add(req.url.split('/').at(-1));
      // Simulate an accepted upload followed by a transient failure.
      if (puts === 1) { res.writeHead(503); res.end('{}'); } else res.end('{}');
    } else if (req.method === 'GET') res.end(JSON.stringify({ manifestSha256: digest, complete: uploaded.size === saved.packages.length }));
    else if (req.url.endsWith('/commit')) { committed = true; assert.equal(JSON.parse(body).manifestSha256, digest); res.end('{}'); }
    else { res.writeHead(404); res.end('{}'); }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => { server.closeAllConnections(); server.close(); });
  const registry = `http://127.0.0.1:${server.address().port}`;
  const first = await invoke(['stage', '--manifest', file, '--registry', registry], dir);
  assert.equal(first.code, 0, first.output); assert.equal(puts, 2); assert.ok(!first.output.includes('test-secret'));
  const state = path.join(dir, '.ppr-tool/state.json');
  if (process.platform !== 'win32') assert.equal((await stat(state)).mode & 0o777, 0o600);
  assert.equal((await invoke(['stage', '--manifest', file, '--registry', registry], dir)).code, 0);
  assert.equal(puts, 2, 'resume skips uploaded packages');
  assert.equal((await invoke(['commit', '--release', 'release-1'], dir)).code, 1);
  assert.equal(committed, false);
  assert.equal((await invoke(['verify', '--release', 'release-1', '--', process.execPath, '-e', 'process.exit(1)'], dir)).code, 1);
  assert.equal((await invoke(['commit', '--release', 'release-1'], dir)).code, 1);
  const verified = await invoke(['verify', '--release', 'release-1', '--', process.execPath, '-e', 'if (!process.env.PPR_PREVIEW || !process.env.PPR_DOWNLOAD_TOKEN) process.exit(2)'], dir);
  assert.equal(verified.code, 0, verified.output);
  const done = await invoke(['commit', '--release', 'release-1'], dir);
  assert.equal(done.code, 0, done.output); assert.equal(committed, true);
  const artifact = path.join(dir, 'pkg.tgz');
  await writeFile(artifact, tar({ 'package/package.json': JSON.stringify({ name: '@example/demo-sources', version: '1.2.3', description: 'changed' }) }));
  const changed = await invoke(['commit', '--release', 'release-1'], dir);
  assert.equal(changed.code, 1); assert.match(changed.output, /changed/);
});
