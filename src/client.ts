import { createReadStream } from 'node:fs';
import { readFile, mkdir, writeFile, rename } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import path from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { prepare, wireManifest, hash, type Prepared } from './manifest.js';

export type State = { schemaVersion: 1; registry: string; releaseId: string; manifest: string; digest: string; product: string; version: string; variant: string; commit: string; token: string; expiresAt?: string; preview: string; testedDigest?: string };
export const mask = (value: string) => {
  if (process.env.GITHUB_ACTIONS === 'true') process.stdout.write(`::add-mask::${value.replaceAll('%', '%25').replaceAll('\r', '%0D').replaceAll('\n', '%0A')}\n`);
};
export function registryUrl(value: string): string {
  const url = new URL(value);
  if (url.username || url.password || url.search || url.hash || url.pathname !== '/') throw new Error('Registry must be an origin without credentials, path or query');
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname))) throw new Error('Registry requires HTTPS');
  return url.origin;
}
export async function saveState(file: string, state: State) {
  await mkdir(path.dirname(file), { recursive: true, mode: 0o700 });
  const temporary = `${file}.${randomUUID()}.tmp`;
  await writeFile(temporary, JSON.stringify(state, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  await rename(temporary, file);
}
export async function loadState(file: string): Promise<State> {
  const state = JSON.parse(await readFile(file, 'utf8')) as State;
  if (!state || state.schemaVersion !== 1 || typeof state.releaseId !== 'string' || !/^[A-Za-z0-9_-]+$/.test(state.releaseId) || !/^[a-f0-9]{64}$/.test(state.digest) || typeof state.token !== 'string' || !state.token || /[\r\n]/.test(state.token)) throw new Error('Invalid publication state');
  const registry = registryUrl(state.registry);
  if (state.preview !== `${registry}/preview/${state.releaseId}` || (state.expiresAt && !Number.isFinite(Date.parse(state.expiresAt)))) throw new Error('Invalid publication state');
  mask(state.token);
  return state;
}

export class Client {
  constructor(public registry: string, public token = '') { this.registry = registryUrl(registry); }
  async request(route: string, method = 'GET', body?: object | (() => ReturnType<typeof createReadStream>), headers: Record<string, string> = {}): Promise<any> {
    for (let attempt = 0; attempt < 4; attempt++) {
      try {
        const stream = typeof body === 'function';
        const response = await fetch(this.registry + route, {
          method, redirect: 'error', signal: AbortSignal.timeout(120_000),
          headers: { ...(this.token ? { authorization: `Bearer ${this.token}` } : {}), ...(body && !stream ? { 'content-type': 'application/json' } : {}), ...headers },
          body: stream ? body() : body ? JSON.stringify(body) : undefined,
          ...(stream ? { duplex: 'half' } : {})
        } as RequestInit);
        if ((response.status === 429 || response.status >= 500) && attempt < 3) {
          const seconds = Number(response.headers.get('retry-after'));
          await response.body?.cancel();
          await sleep(Number.isFinite(seconds) && seconds > 0 ? Math.min(seconds * 1000, 10_000) : 200 * 2 ** attempt);
          continue;
        }
        if (!response.ok) {
          await response.body?.cancel();
          // Do not print untrusted response bodies; they could echo tokens or signed URLs.
          throw new Error(`Registry ${method} failed with HTTP ${response.status}`);
        }
        if (response.status === 204) return {};
        const chunks: Uint8Array[] = [];
        let size = 0;
        if (response.body) for await (const chunk of response.body) {
          size += chunk.length;
          if (size > 4 * 1024 * 1024) throw new Error('Oversized registry response');
          chunks.push(chunk);
        }
        return JSON.parse(Buffer.concat(chunks).toString('utf8'));
      } catch (error) {
        if (attempt < 3 && (error instanceof TypeError || (error instanceof Error && ['TimeoutError', 'AbortError'].includes(error.name)))) {
          await sleep(200 * 2 ** attempt); continue;
        }
        throw error;
      }
    }
    throw new Error('Request retry limit exceeded');
  }
}

async function authenticate(client: Client, release: Pick<Prepared, 'product' | 'version' | 'variant' | 'commit'>): Promise<{ token: string; expiresAt?: string }> {
  if (process.env.PPR_TOKEN) { mask(process.env.PPR_TOKEN); return { token: process.env.PPR_TOKEN }; }
  const endpoint = process.env.ACTIONS_ID_TOKEN_REQUEST_URL;
  const requestToken = process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
  if (!endpoint || !requestToken) throw new Error('Set PPR_TOKEN or enable GitHub Actions id-token: write');
  const url = new URL(endpoint);
  if (url.protocol !== 'https:' || !url.hostname.endsWith('.actions.githubusercontent.com')) throw new Error('Unexpected GitHub OIDC endpoint');
  url.searchParams.set('audience', client.registry);
  const response = await fetch(url, { redirect: 'error', headers: { authorization: `Bearer ${requestToken}` }, signal: AbortSignal.timeout(30_000) });
  if (!response.ok) throw new Error(`GitHub OIDC failed with HTTP ${response.status}`);
  const { value } = await response.json() as { value: string };
  if (typeof value !== 'string' || !value) throw new Error('GitHub returned no OIDC token');
  mask(value);
  const { product, version, variant, commit } = release;
  const credentials = await client.request('/api/v1/auth/oidc', 'POST', { token: value, product, version, variant, commit });
  if (typeof credentials.token !== 'string' || !credentials.token || !Number.isFinite(Date.parse(credentials.expiresAt)) || Date.parse(credentials.expiresAt) <= Date.now()) throw new Error('Invalid publication session');
  mask(credentials.token);
  return credentials;
}
const routeFor = (product: string, id = '') => `/api/v1/products/${encodeURIComponent(product)}/releases${id ? '/' + encodeURIComponent(id) : ''}`;

export async function stage(manifest: string, registry: string, stateFile: string) {
  const { release, digest, paths } = await prepare(manifest);
  const client = new Client(registry);
  const credentials = await authenticate(client, release);
  client.token = credentials.token;
  const staged = await client.request(routeFor(release.product), 'POST', wireManifest(release), { 'idempotency-key': digest });
  if (typeof staged.id !== 'string' || !/^[A-Za-z0-9_-]+$/.test(staged.id) || staged.manifestSha256 !== digest) throw new Error('Registry returned a different release identity');
  const preview = `${client.registry}/preview/${staged.id}`;
  const state: State = { schemaVersion: 1, registry: client.registry, releaseId: staged.id, manifest: path.resolve(manifest), digest,
    product: release.product, version: release.version, variant: release.variant, commit: release.commit, ...credentials, preview };
  await saveState(stateFile, state);
  const uploaded = new Set(Array.isArray(staged.uploaded) ? staged.uploaded : []);
  for (const pkg of release.packages) {
    if (uploaded.has(pkg.key)) continue;
    const file = paths.get(pkg.key)!;
    if (hash(await readFile(file)) !== pkg.sha256) throw new Error(`Artifact changed after validation: ${pkg.name}`);
    await client.request(`${routeFor(release.product, staged.id)}/artifacts/${pkg.key}`, 'PUT', () => createReadStream(file), {
      'content-type': 'application/octet-stream', 'content-length': String(pkg.size), 'x-content-sha256': pkg.sha256, 'idempotency-key': `${digest}:${pkg.key}`
    });
    console.log(`Uploaded ${pkg.format} ${pkg.name}@${pkg.version}`);
  }
  const status = await client.request(routeFor(release.product, staged.id));
  if (status.manifestSha256 !== digest || status.complete !== true) throw new Error('Release is incomplete; rerun stage to resume');
  console.log(`Staged release ${staged.id}; preview ${preview}`);
  return state;
}

export async function authenticatedState(stateFile: string, expectedId: string) {
  const state = await loadState(stateFile);
  if (state.releaseId !== expectedId) throw new Error('Release does not match local publication state');
  const registry = process.env.PPR_REGISTRY;
  if (registry && registryUrl(registry) !== state.registry) throw new Error('Registry does not match local publication state');
  const client = new Client(state.registry, state.token);
  if (state.expiresAt && Date.parse(state.expiresAt) < Date.now() + 30_000) {
    client.token = '';
    const credentials = await authenticate(client, state);
    Object.assign(state, credentials);
    client.token = state.token;
    await saveState(stateFile, state);
  }
  return { state, client };
}

export async function commit(stateFile: string, id: string) {
  const { state, client } = await authenticatedState(stateFile, id);
  const { digest } = await prepare(state.manifest);
  if (digest !== state.digest) throw new Error('Artifacts changed after staging');
  if (state.testedDigest !== digest) throw new Error('Run ppr-tool verify --release ID -- COMMAND before commit');
  const status = await client.request(routeFor(state.product, id));
  if (status.manifestSha256 !== digest || status.complete !== true) throw new Error('Registry release does not match tested artifacts');
  await client.request(`${routeFor(state.product, id)}/commit`, 'POST', { manifestSha256: digest }, { 'idempotency-key': `commit:${digest}` });
  console.log(`Published ${state.product} ${state.version} (${state.variant}), release ${id}`);
}
