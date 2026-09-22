export type Format = 'npm' | 'nuget' | 'composer' | 'cargo';
export type PackageInput = { format: Format; name: string; version: string; file: string };
export type Manifest = { schemaVersion: 1; product: string; version: string; variant: string; commit: string; packages: PackageInput[] };
export type Artifact = PackageInput & { key: string; size: number; sha256: string; sha512: string; sha1: string; metadata: Record<string, unknown> };
export type Prepared = Omit<Manifest, 'packages'> & { packages: Artifact[] };

export const packageNamePatterns: Record<Format, RegExp> = { npm: /^(?:@[a-z0-9][a-z0-9._-]*\/)?[a-z0-9][a-z0-9._-]*$/, composer: /^[a-z0-9][a-z0-9_.-]*\/[a-z0-9][a-z0-9_.-]*$/, nuget: /^[A-Za-z0-9][A-Za-z0-9_.-]*$/, cargo: /^[A-Za-z][A-Za-z0-9_-]*$/ };
export const formatName = (format: string, name: string) => format === 'nuget' ? name.toLowerCase() : name;
export type WireManifest = Omit<Manifest, 'packages'> & { packages: Omit<Artifact, 'file'>[] };
export type StageResult = { id: string; manifestSha256: string; uploaded: string[] };
export type ReleaseStatus = { manifestSha256: string; complete: boolean };
