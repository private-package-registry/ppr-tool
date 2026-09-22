import test from 'node:test';
import assert from 'node:assert/strict';
import { build } from 'esbuild';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import path from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

// Bundle to exercise the actual auth implementation with a controlled HTTP boundary.
const result = await build({ entryPoints: [fileURLToPath(new URL('../src/client.ts', import.meta.url))], bundle: true, write: false, format: 'esm', platform: 'node' });
const { authenticatedState, saveState, Client, registryUrl } = await import(`data:text/javascript;base64,${Buffer.from(result.outputFiles[0].text).toString('base64')}`);

test('OIDC refresh exchanges the new JWT with only the release scope', async t => {
  const dir = await mkdtemp(path.join(tmpdir(), 'ppr-tool-oidc-'));
  const file = path.join(dir, 'state.json');
  t.after(() => rm(dir, { recursive: true, force: true }));
  const state = { schemaVersion: 1, registry: 'https://registry.example.test', releaseId: 'draft', manifest: 'private-local-path', digest: 'a'.repeat(64), product: 'demo', version: '1.2.3', variant: 'sources', commit: 'b'.repeat(40), token: 'expired-session', expiresAt: '2000-01-01T00:00:00Z', preview: 'https://registry.example.test/preview/draft' };
  await saveState(file, state);
  const env = { ...process.env }, originalFetch = globalThis.fetch;
  t.after(() => { process.env = env; globalThis.fetch = originalFetch; });
  delete process.env.PPR_TOKEN; delete process.env.PPR_REGISTRY;
  process.env.ACTIONS_ID_TOKEN_REQUEST_URL = 'https://run.actions.githubusercontent.com/oidc?existing=yes';
  process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN = 'runner-token';
  process.env.GITHUB_ACTIONS = '';
  let calls = 0;
  globalThis.fetch = async (input, options) => {
    calls++;
    const url = new URL(input);
    if (calls === 1) {
      assert.equal(url.searchParams.get('audience'), state.registry);
      assert.equal(url.searchParams.get('existing'), 'yes');
      assert.equal(options.headers.authorization, 'Bearer runner-token');
      return Response.json({ value: 'fresh-jwt' });
    }
    assert.equal(url.href, `${state.registry}/api/v1/auth/oidc`);
    assert.deepEqual(JSON.parse(options.body), { token: 'fresh-jwt', product: 'demo', version: '1.2.3', variant: 'sources', commit: 'b'.repeat(40) });
    return Response.json({ token: 'fresh-session', expiresAt: new Date(Date.now() + 3600000).toISOString() });
  };
  assert.equal((await authenticatedState(file, 'draft')).state.token, 'fresh-session');
  assert.equal(calls, 2);
  process.env.ACTIONS_ID_TOKEN_REQUEST_URL = 'https://untrusted.example.test/';
  await saveState(file, state);
  await assert.rejects(authenticatedState(file, 'draft'), /Unexpected GitHub/);
});

test('reject credentials and remote plaintext origins; never echo API error bodies', async t => {
  for (const url of ['http://example.com', 'https://secret@example.com', 'https://example.com/path', 'https://example.com?token=x']) assert.throws(() => registryUrl(url));
  const original = globalThis.fetch;
  t.after(() => globalThis.fetch = original);
  globalThis.fetch = async () => new Response('secret-in-body', { status: 403 });
  await assert.rejects(new Client('https://example.test').request('/api/v1/test'), error => error.message === 'Registry GET failed with HTTP 403');
});
