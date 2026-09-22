import { parseArgs } from 'node:util';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { appendFile, writeFile, mkdir } from 'node:fs/promises';
import { prepare } from './manifest.js';
import { stage, commit, authenticatedState, saveState } from './client.js';

async function main() {
  const [command, ...args] = process.argv.slice(2);
  if (!command || command === '--help' || command === 'help') {
    console.log('ppr-tool 0.1.0\nvalidate --manifest FILE\nstage --manifest FILE [--registry URL] [--state FILE]\nverify --release ID [--state FILE] -- COMMAND [ARGS...]\ncommit --release ID [--state FILE]\nCredentials: PPR_TOKEN or GitHub Actions OIDC. Registry: PPR_REGISTRY.');
    return;
  }
  const separator = args.indexOf('--');
  const verifyCommand = separator >= 0 ? args.slice(separator + 1) : [];
  const { values } = parseArgs({ args: separator >= 0 ? args.slice(0, separator) : args, options: {
    manifest: { type: 'string' }, registry: { type: 'string' }, state: { type: 'string' }, release: { type: 'string' }
  } });
  const stateFile = path.resolve(values.state || process.env.PPR_STATE || '.ppr-tool/state.json');
  if (command === 'validate') {
    if (!values.manifest) throw new Error('--manifest is required');
    const result = await prepare(values.manifest);
    console.log(`Valid: ${result.release.packages.length} packages; SHA-256 ${result.digest}`);
  } else if (command === 'stage') {
    if (!values.manifest) throw new Error('--manifest is required');
    const state = await stage(values.manifest, values.registry || process.env.PPR_REGISTRY || '', stateFile);
    if (process.env.GITHUB_OUTPUT) await appendFile(process.env.GITHUB_OUTPUT, `release-id=${state.releaseId}\npreview-url=${state.preview}\n`);
    if (process.env.GITHUB_ENV) await appendFile(process.env.GITHUB_ENV, `PPR_RELEASE_ID=${state.releaseId}\n`);
  } else if (command === 'verify') {
    if (!values.release || !verifyCommand.length) throw new Error('verify requires --release ID -- COMMAND');
    const { state } = await authenticatedState(stateFile, values.release);
    const { release, digest } = await prepare(state.manifest);
    if (digest !== state.digest) throw new Error('Artifacts changed after staging');
    delete state.testedDigest;
    await saveState(stateFile, state);
    const previewFile = path.join(path.dirname(stateFile), 'preview.json');
    await mkdir(path.dirname(previewFile), { recursive: true });
    await writeFile(previewFile, JSON.stringify({ releaseId: state.releaseId, baseUrl: state.preview, cargoIndex: `${state.registry}/cargo/index/`, packages: release.packages.map(({ format, name, version }) => ({ format, name, version })) }, null, 2));
    const code = await new Promise<number | null>((resolve, reject) => {
      const child = spawn(verifyCommand[0], verifyCommand.slice(1), { stdio: 'inherit', shell: false,
        env: { ...process.env, PPR_PREVIEW: previewFile, PPR_DOWNLOAD_TOKEN: state.token } });
      child.on('error', reject); child.on('exit', resolve);
    });
    if (code !== 0) throw new Error(`Package verification failed (exit ${code})`);
    if ((await prepare(state.manifest)).digest !== digest) throw new Error('Verification changed release artifacts');
    state.testedDigest = digest;
    await saveState(stateFile, state);
    console.log(`Verified release ${state.releaseId}`);
  } else if (command === 'commit') {
    if (!values.release) throw new Error('--release is required');
    await commit(stateFile, values.release);
  } else throw new Error(`Unknown command: ${command}`);
}
main().catch(error => { console.error(`ppr-tool: ${error instanceof Error ? error.message : 'Operation failed'}`); process.exitCode = 1; });
