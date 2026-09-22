# ppr-tool

Publisher CLI for a private package registry. It validates a release manifest and finished archives,
stages them as an invisible draft, runs your installation tests against the draft, and commits the release.
Supported formats: npm `.tgz`, NuGet `.nupkg`, Composer ZIP with root `composer.json`, Cargo `.crate`.

This repository is a read-only mirror of the `tool/` module in the private monorepo. Releases are
published here; report issues to the maintainers rather than opening pull requests against the mirror.

## Install

Node.js 22+ is required. Download `ppr-tool.mjs` from a [release](https://github.com/private-package-registry/ppr-tool/releases)
and verify it against the `SHA256SUMS` asset, or use the
[setup-ppr-tool-action](https://github.com/private-package-registry/setup-ppr-tool-action) on GitHub Actions.
The bundle has no runtime dependencies.

Development: `npm ci`, `npm run typecheck`, `npm test`. Run `npm run build` and commit `dist/` when changing the CLI.

## Usage

```sh
export PPR_REGISTRY=https://registry.example.com
# Locally: supply PPR_TOKEN through your secret manager/environment.
# On GitHub Actions: omit PPR_TOKEN and grant id-token: write.
ppr-tool validate --manifest dist/release.json
ppr-tool stage --manifest dist/release.json
ppr-tool verify --release RELEASE_ID -- node tools/test-install.mjs
ppr-tool commit --release RELEASE_ID
```

`stage` prints the release ID. On Actions it also writes `release-id` and `preview-url` step outputs,
and exports `PPR_RELEASE_ID` for subsequent steps. Repeating it with the same manifest and
archives resumes the server draft, skips completed uploads and invalidates the previous local test result.
A failed upload leaves the draft invisible. Retry it; never rebuild artifacts to resume the same draft.
The server rejects attempts to reuse a product/version/variant with different content.

`verify` executes an explicit command (never commands from the manifest). It provides:

- `PPR_PREVIEW`: path to a JSON file containing `releaseId`, `baseUrl`, `cargoIndex` and package identities;
- `PPR_DOWNLOAD_TOKEN`: current publication session credential for private draft downloads.

The product command configures each package manager and tests installation in a clean directory.
Successful verification records the tested manifest digest locally. `commit` checks it and the server's
completeness before requesting publication. This is an accidental-publish guard, not server-side proof
that tests ran: authorized API clients can publish directly.

## Credentials and state

Authentication is lazy. `stage` uses `PPR_TOKEN` when set; otherwise, on GitHub Actions with
`id-token: write`, it requests an OIDC token with the registry origin as audience and exchanges it for a
short-lived publication session. Tokens take precedence over OIDC. Sessions refresh before
verification/commit when near expiry; rerun `stage` after an upload-time expiration.
Never pass tokens in command arguments or a manifest.

State defaults to `.ppr-tool/state.json`; override with `--state` or `PPR_STATE`. **State contains a
credential** (mode 0600 on POSIX). Keep it out of Git, caches, logs and uploaded artifacts; delete it
when finished. Concurrent invocations must use separate state files.

## Manifest

[release.schema.json](release.schema.json) defines the local contract. Example:

```json
{
  "schemaVersion": 1,
  "product": "example-product",
  "version": "1.2.3",
  "variant": "sources",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "packages": [
    { "format": "npm", "name": "@example/client-sources", "version": "1.2.3", "file": "npm/client.tgz" }
  ]
}
```

Files are relative to the manifest and must resolve inside its directory. Archives are capped at
64 MiB compressed and expanded. Every package uses the release's stable `X.Y.Z` version.
The CLI extracts registry metadata from archives, checks their identities, and computes SHA-256,
SHA-512 and SHA-1. SHA-1 is only for legacy package-manager metadata; SHA-256 binds publication.

Only product-owned build scripts decide naming, dependencies, source contents and platform targets.
`variant` is extensible (for example `no-sources`); it does not change compiler behavior in the CLI.
No build commands or secrets belong in manifests. Uploads are whole files with retries,
not multipart/resumable byte ranges.

## License

[MIT](LICENSE)
