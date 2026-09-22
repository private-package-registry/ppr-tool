import { createHash } from 'node:crypto';
import { readFile, realpath, stat } from 'node:fs/promises';
import path from 'node:path';
import { gunzipSync } from 'node:zlib';
import { unzipSync } from 'fflate';
import { parsePackageMetadata } from './shared/metadata.js';

import { packageNamePatterns, formatName, type Format, type PackageInput, type Manifest, type Artifact, type Prepared } from './shared/manifest.js';
export type { Format, PackageInput, Manifest, Artifact, Prepared } from './shared/manifest.js';
export const hash = (data: string | Buffer, algorithm = 'sha256', encoding: 'hex' | 'base64' = 'hex') => createHash(algorithm).update(data).digest(encoding);
const MAX = 64 * 1024 * 1024;
const safeName = (name: string) => !name.includes('\\') && !name.startsWith('/') && !name.split('/').includes('..');
const text = (data: Uint8Array) => Buffer.from(data).toString('utf8');

function entries(buffer: Buffer, zipped: boolean): Map<string, Uint8Array> {
  if (zipped) {
    let total = 0;
    const seen = new Set<string>();
    const files = unzipSync(buffer, { filter: (entry) => {
      total += entry.originalSize;
      if (!safeName(entry.name) || total > MAX || seen.has(entry.name)) throw new Error('Unsafe or oversized ZIP contents');
      seen.add(entry.name);
      return true;
    } });
    return new Map(Object.entries(files));
  }
  const tar = gunzipSync(buffer, { maxOutputLength: MAX });
  const result = new Map<string, Uint8Array>();
  for (let offset = 0; offset + 512 <= tar.length;) {
    const header = tar.subarray(offset, offset + 512);
    const str = (start: number, length: number) => header.subarray(start, start + length).toString().replace(/\0.*$/s, '');
    if (!header.some(Boolean)) break;
    const size = parseInt(str(124, 12).trim() || '0', 8);
    const prefix = str(345, 155);
    const name = (prefix ? `${prefix}/` : '') + str(0, 100);
    if (!Number.isSafeInteger(size) || size < 0 || offset + 512 + size > tar.length || !safeName(name)) throw new Error('Invalid TAR contents');
    const kind = str(156, 1);
    if (kind === '1' || kind === '2') throw new Error('Archive links are not supported');
    if (kind === '0' || kind === '') {
      if (result.has(name)) throw new Error('Duplicate archive entry');
      result.set(name, tar.subarray(offset + 512, offset + 512 + size));
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  return result;
}

export function packageMetadata(format: Format, buffer: Buffer): Record<string, any> {
  const files = entries(buffer, format === 'composer' || format === 'nuget');
  const get = (name: string) => {
    const value = files.get(name);
    if (!value) throw new Error(`Archive missing ${name}`);
    if (value.length > 256 * 1024) throw new Error('Package metadata exceeds 256 KiB');
    return text(value);
  };
  if (format === 'npm') return parsePackageMetadata(format, get('package/package.json'));
  if (format === 'composer') return parsePackageMetadata(format, get('composer.json'));
  const names = [...files.keys()].filter(name => format === 'cargo' ? /^[^/]+\/Cargo\.toml$/.test(name) : /^[^/]+\.nuspec$/i.test(name));
  if (names.length !== 1) throw new Error('Expected one package metadata file');
  return parsePackageMetadata(format, get(names[0]));
}

export async function prepare(file: string): Promise<{ release: Prepared; digest: string; paths: Map<string, string> }> {
  const root = await realpath(path.dirname(path.resolve(file)));
  const input = JSON.parse(await readFile(file, 'utf8')) as Manifest;
  if (!input || typeof input !== 'object' || ['product', 'version', 'variant', 'commit'].some(field => typeof (input as any)[field] !== 'string') || input.schemaVersion !== 1 || !/^[a-z0-9][a-z0-9-]{0,63}$/.test(input.product) ||
      !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(input.version) ||
      !/^[a-z0-9][a-z0-9-]{0,31}$/.test(input.variant) || !/^[a-f0-9]{40}$/.test(input.commit) ||
      !Array.isArray(input.packages) || !input.packages.length) throw new Error('Invalid release manifest');
  if (Object.keys(input).some(key => !['schemaVersion', 'product', 'version', 'variant', 'commit', 'packages'].includes(key))) throw new Error('Unknown release manifest field');
  const paths = new Map<string, string>();
  const packages: Artifact[] = [];
  const identities = new Set<string>();
  for (const pkg of input.packages) {
    if (!pkg || !['npm', 'nuget', 'composer', 'cargo'].includes(pkg.format) || typeof pkg.name !== 'string' ||
        !pkg.name || pkg.version !== input.version || typeof pkg.file !== 'string' || !safeName(pkg.file)) throw new Error('Invalid package entry');
    if (Object.keys(pkg).some(key => !['format', 'name', 'version', 'file'].includes(key))) throw new Error('Unknown package entry field');
    if (!packageNamePatterns[pkg.format].test(pkg.name)) throw new Error('Invalid package name');
    const absolute = await realpath(path.resolve(root, pkg.file));
    if (!absolute.startsWith(root + path.sep)) throw new Error('Artifact must be inside manifest directory');
    const info = await stat(absolute);
    if (!info.isFile() || info.size > MAX) throw new Error('Artifact must be a file no larger than 64 MiB');
    const identity = `${pkg.format}:${formatName(pkg.format, pkg.name)}:${pkg.version}`;
    if (identities.has(identity)) throw new Error(`Duplicate package: ${identity}`);
    identities.add(identity);
    const buffer = await readFile(absolute);
    const metadata = packageMetadata(pkg.format, buffer);
    if (!metadata || typeof metadata !== 'object' || Array.isArray(metadata)) throw new Error('Invalid package metadata');
    if (metadata.name !== pkg.name || metadata.version !== pkg.version) throw new Error(`Archive identity differs from manifest: ${pkg.name}`);
    if (metadata.private === true) throw new Error(`Private workspace package cannot be published: ${pkg.name}`);
    if (pkg.format === 'npm') for (const section of ['dependencies', 'optionalDependencies', 'peerDependencies']) {
      for (const value of Object.values(metadata[section] || {})) if (typeof value !== 'string' || /^(workspace:|file:|link:)/.test(value)) throw new Error(`Unresolved local dependency in ${pkg.name}`);
    }
    const key = hash(identity);
    paths.set(key, absolute);
    packages.push({ format: pkg.format, name: pkg.name, version: pkg.version, file: pkg.file, key, size: buffer.length,
      sha256: hash(buffer), sha512: hash(buffer, 'sha512', 'base64'), sha1: hash(buffer, 'sha1'), metadata });
  }
  packages.sort((a, b) => a.key.localeCompare(b.key));
  const release = { schemaVersion: 1 as const, product: input.product, version: input.version, variant: input.variant, commit: input.commit, packages };
  // Paths are local implementation details, not part of the immutable release identity.
  const digest = hash(JSON.stringify(wireManifest(release)));
  return { release, digest, paths };
}

export function wireManifest(release: Prepared) {
  return { ...release, packages: release.packages.map(({ file: _file, ...pkg }) => pkg) };
}
