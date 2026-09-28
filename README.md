# ppr-tool

Publisher CLI for a private package registry. It inspects finished archives, stages them as an
invisible draft, runs your installation tests against the draft, and commits the release.
Supported formats: npm `.tgz`, NuGet `.nupkg`, Composer ZIP with root `composer.json`, Cargo `.crate`.

`ppr-tool` is a single static binary with no runtime dependencies. You do not need Node.js.

This repository is a read-only mirror of the `tool/` module in the private monorepo. Releases are
published here; report issues to the maintainers rather than opening pull requests against the mirror.

## Install

Download the asset for your platform from a [release](https://github.com/private-package-registry/ppr-tool/releases)
and verify it against `SHA256SUMS`:

| Platform | Asset |
|---|---|
| Linux x64 (any distribution, static musl) | `ppr-tool-x86_64-unknown-linux-musl` |
| Linux ARM64 (static musl) | `ppr-tool-aarch64-unknown-linux-musl` |
| macOS Intel | `ppr-tool-x86_64-apple-darwin` |
| macOS Apple silicon | `ppr-tool-aarch64-apple-darwin` |
| Windows x64 | `ppr-tool-x86_64-pc-windows-msvc.exe` |

```sh
curl -fsSLO https://github.com/private-package-registry/ppr-tool/releases/download/v1.0.0/ppr-tool-x86_64-unknown-linux-musl
curl -fsSLO https://github.com/private-package-registry/ppr-tool/releases/download/v1.0.0/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
install -m 0755 ppr-tool-x86_64-unknown-linux-musl /usr/local/bin/ppr-tool
```

ppr-tool 1.x speaks publication API v2 (presigned uploads) and needs a registry that implements it.
Releases before 1.0 use the withdrawn v1 API and cannot publish to current registries.

On GitHub Actions, use [setup-ppr-tool-action](https://github.com/private-package-registry/setup-ppr-tool-action).
To build from source you need stable Rust: `cargo build --release --locked`, which produces `target/release/ppr-tool`.

## Usage

```sh
export PPR_REGISTRY=https://registry.example.com
export PPR_PRODUCT=example-product PPR_VARIANT=sources
# Locally: supply PPR_TOKEN through your secret manager. On GitHub Actions: omit it and grant id-token: write.

ppr-tool validate --commit "$(git rev-parse HEAD)" dist/npm/*.tgz dist/nuget/*.nupkg dist/*.crate
ppr-tool stage    --commit "$(git rev-parse HEAD)" dist/npm/*.tgz dist/nuget/*.nupkg dist/*.crate
ppr-tool verify -- node tools/test-install.mjs
ppr-tool commit

# or all three steps at once:
ppr-tool publish --commit "$(git rev-parse HEAD)" dist/npm/*.tgz dist/nuget/*.nupkg -- node tools/test-install.mjs
```

| Command | What it does |
|---|---|
| `validate ARCHIVE...` | Reads the archives offline and prints the release. `--json` prints the exact manifest bytes that `stage` would send |
| `stage ARCHIVE...` | Reserves or resumes the draft and uploads missing archives in parallel (`-j N`, default 4) straight to the registry's object storage |
| `verify -- COMMAND...` | Runs your installation test against the draft and records the result |
| `commit` | Publishes a verified draft |
| `status` | Shows the local state and the registry's view of the draft, including per-archive upload states. It never prints the credential |
| `publish ARCHIVE... -- COMMAND...` | Runs `stage`, `verify` and `commit` in one process |

Archives are positional arguments. `ppr-tool` expands glob patterns itself, so quoting works the same
on every shell, including Windows. A pattern that matches nothing is an error. Each file's format comes
from its extension: `.tgz`/`.tar.gz`, `.crate`, `.nupkg` or `.zip`. Each package's name and version come
from its metadata. Every archive must carry the same stable `X.Y.Z` version, which becomes the release
version (`--version X.Y.Z` adds a cross-check). The order of arguments never changes the result.

`verify`, `commit` and `status` read the release from the state file. `--release ID` and `--registry URL`
are optional cross-checks against it.

### Options and environment

| Flag | Environment | Meaning |
|---|---|---|
| `--registry URL` | `PPR_REGISTRY` | Registry origin: HTTPS, or HTTP on loopback only |
| `--product NAME` | `PPR_PRODUCT` | Product slug |
| `--variant NAME` | `PPR_VARIANT` | Release variant, for example `sources` or `no-sources` |
| `--commit SHA` | `PPR_COMMIT` | Full source commit; on GitHub Actions it defaults to `GITHUB_SHA` |
| `--state FILE` | `PPR_STATE` | Publication state (default `.ppr-tool/state.json`) |
| (none) | `PPR_TOKEN` | Static publication token; takes precedence over GitHub OIDC |
| `--json` | (none) | Machine-readable output on stdout; errors become `{"error": {...}}` |
| `-q`, `--quiet` | (none) | Only results and errors |
| `--color auto\|always\|never` | `NO_COLOR` | Colours; they are removed automatically when output is not a terminal |

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Internal error |
| 2 | Usage error: unknown flag, missing `--product`/`--variant`/`--commit`/`--registry`, or missing `-- COMMAND` |
| 3 | Local validation failed: extension, glob, archive safety, metadata, name, version, duplicate, `private`, local dependency or size limit |
| 4 | Authentication: no credential, or the OIDC exchange failed, or the registry returned 401/403. Registry HTTP errors add `httpStatus` (and the registry's error `code`, when it sent one) under `--json` |
| 5 | Registry, object storage or network failure after retries, unexpected status, redirect, rejected archive or incomplete release. Object storage errors add `storageStatus` under `--json` |
| 6 | The verification command failed. The message includes its exit code, and `--json` adds `childExitCode` |
| 7 | State mismatch: missing or invalid state, `--release`/`--registry` differs, artifacts changed since `stage`, or `commit` before a successful `verify` |

Errors print as `ppr-tool: message` with an optional `hint:` line, and name the file and field at fault,
for example `nuget/Foo.1.2.3.nupkg: Foo.nuspec: <id> must contain text only`. Response bodies from the
registry and from object storage are never printed, and neither are upload URLs.

## Staging, resuming and verification

Running `stage` again with the same archives resumes the draft. It skips completed uploads and
invalidates the previous local test result. A failed upload leaves the draft invisible: retry it, and
never rebuild artifacts to resume the same draft. The server rejects any attempt to reuse a
product/version/variant with different content.

Archives do not pass through the registry (publication API v2). For each archive the registry still
needs, `stage`:

1. asks the registry for a short-lived presigned upload URL;
2. streams the archive from disk to that URL with a plain `PUT`. This request goes to the registry's
   object storage and never carries the registry credential or cookies. Upload URLs must be HTTPS;
   plain HTTP is accepted only for loopback storage when the registry itself runs on a loopback
   origin (local development);
3. asks the registry to verify the stored object. The registry checks size, hashes, archive rules and
   metadata before it accepts the archive.

Network failures, HTTP 429 and 5xx are retried (four attempts, the storage upload on the same URL).
If the object did not arrive (HTTP 412), or the storage refused an expired URL (HTTP 403, or by
dropping the connection on every attempt), `stage` requests one fresh URL for that archive and uploads
it again. When the registry verifies an archive
asynchronously (HTTP 202), `stage` polls the draft's status every 2 seconds for up to 10 minutes.
The machine running `ppr-tool` must be able to reach the object storage host as well as the registry.

`verify` runs an explicit command, not a shell, so Windows `.cmd`/`.bat` shims need `cmd /c`. The command receives:

- `PPR_PREVIEW`: absolute path to a JSON file containing `releaseId`, `baseUrl`, `cargoIndex` and package identities;
- `PPR_PREVIEW_URL`: the draft's preview base URL;
- `PPR_RELEASE_ID`: the draft release ID;
- `PPR_DOWNLOAD_TOKEN`: the current publication session credential for private draft downloads.

The product's command configures each package manager and tests installation in a clean directory.
A successful verification records the tested manifest digest locally. `commit` checks that digest and
the server's completeness before requesting publication. This guards against accidental publishing. It
is not server-side proof that tests ran, because authorized API clients can publish directly.

On GitHub Actions, `stage` writes the `release-id` and `preview-url` step outputs, exports
`PPR_RELEASE_ID`, groups upload logs and adds a package table to the job summary. Failures become
`::error` annotations. Workflow commands go to stderr, so `--json` output on stdout stays clean.

## Manifest digest

The tool builds the wire manifest from the archives:

- `schemaVersion`, `product`, `version`, `variant`, `commit`
- `packages`, sorted by key, each with `format`, `name`, `version`, `key`, `size`, `sha256`, `sha512`, `sha1` and `metadata`

It serialises this manifest as [RFC 8785](https://www.rfc-editor.org/rfc/rfc8785) canonical JSON. The
digest is the SHA-256 of those bytes. `ppr-tool validate --json | sha256sum` reproduces it.

Archives are checked with the same rules the registry applies before anything is uploaded:

- safe paths only;
- plain ustar or unencrypted stored/deflate ZIP;
- at most 64 MiB compressed and expanded;
- exactly one metadata file.

The extracted metadata matches, byte for byte, what the registry computes when it re-reads each
archive. This is covered by the conformance fixtures in `tests/fixtures`.

## Credentials and state

Authentication is lazy:

1. `stage` uses `PPR_TOKEN` when it is set.
2. Otherwise, on GitHub Actions with `id-token: write`, it requests an OIDC token with the registry origin as audience.
3. It then exchanges that token for a short-lived publication session.

Sessions refresh before verification or commit when they are within 30 seconds of expiry. If a session
expires during upload, rerun `stage`. Never pass tokens as command arguments. Presigned upload URLs are
short-lived credentials too; `ppr-tool` never prints them.

**The state file contains a credential.** It is written atomically with mode 0600 in a 0700 directory
on POSIX. Keep it out of Git, caches, logs and uploaded artifacts, and delete it when you are finished.
Concurrent invocations must use separate state files.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked
```

`tests/conformance.rs` compares the metadata converters with `tests/fixtures/**/expected.json`. The
fixtures are frozen; the monorepo writes new cases from the registry's own parser
(`php artisan registry:metadata-fixture --write`). `tests/flow.rs` runs the binary against a mock
registry that implements publication API v2, including a mock object store that fails the test if an
upload ever carries a credential.

## License

[MIT](LICENSE)
