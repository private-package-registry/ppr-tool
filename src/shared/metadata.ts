import { XMLParser } from 'fast-xml-parser';
import { parse as parseToml } from 'smol-toml';
import type { Format } from './manifest.js';
// Pure metadata interpretation, shared by the filesystem CLI and streaming Worker.
export function parsePackageMetadata(format: Format, text: string): Record<string, any> {
  let metadata: Record<string, any>;
  if (format === 'npm' || format === 'composer') metadata = JSON.parse(text);
  else if (format === 'cargo') {
    const value = parseToml(text); const pkg = value.package as Record<string, unknown>;
    metadata = { ...value, name: pkg?.name, version: pkg?.version };
  } else {
    if (/<!DOCTYPE|<!ENTITY/i.test(text)) throw new Error('DTD is not supported');
    const value = new XMLParser({ ignoreAttributes: false, parseTagValue: false }).parse(text).package?.metadata;
    if (!value) throw new Error('Missing nuspec metadata');
    metadata = { ...value, name: value.id, version: value.version };
  }
  if (!metadata || Array.isArray(metadata) || typeof metadata !== 'object' || typeof metadata.name !== 'string' || typeof metadata.version !== 'string') throw new Error('Invalid package identity');
  return metadata;
}
