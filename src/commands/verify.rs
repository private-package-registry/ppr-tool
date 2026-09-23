use super::{Context, open_session, preview_path, reprepare};
use crate::cli::{RegistryArgs, ReleaseArgs, StateArgs, VerifyCommandArgs};
use crate::error::{Error, Kind, Result};
use crate::state::{self, State};
use serde_json::json;
use std::path::Path;

pub fn run(ctx: &Context, state_args: &StateArgs, release: &ReleaseArgs, registry: &RegistryArgs, verify: &VerifyCommandArgs) -> Result<()> {
    let (path, state_value, _client) = open_session(ctx, state_args, release, registry)?;
    run_with_state(ctx, &path, state_value, &verify.command, false)
}

/// With `quiet_stdout`, the command's stdout is sent to stderr so machine-readable output stays clean.
pub fn run_with_state(ctx: &Context, path: &Path, mut state_value: State, command: &[String], quiet_stdout: bool) -> Result<()> {
    let ui = &ctx.ui;
    let prepared = reprepare(&state_value)?;
    state_value.tested_digest = None;
    state::save(path, &state_value)?;
    let preview_file = preview_path(path);
    let preview = json!({
        "releaseId": state_value.release_id,
        "baseUrl": state_value.preview,
        "cargoIndex": format!("{}/cargo/index/", state_value.registry),
        "packages": prepared.preview_packages(),
    });
    std::fs::write(&preview_file, format!("{}\n", serde_json::to_string_pretty(&preview)?))
        .map_err(|e| Error::internal(format!("cannot write {}: {e}", preview_file.display())))?;
    let program = &command[0];
    ui.info(&format!("Verifying release {} with: {}", state_value.release_id, command.join(" ")));
    let mut child = std::process::Command::new(program);
    if quiet_stdout {
        child.stdout(std::io::stderr());
    }
    let status = child
        .args(&command[1..])
        .env("PPR_PREVIEW", &preview_file)
        .env("PPR_PREVIEW_URL", &state_value.preview)
        .env("PPR_RELEASE_ID", &state_value.release_id)
        .env("PPR_DOWNLOAD_TOKEN", &state_value.token)
        .status()
        .map_err(|e| {
            Error::new(Kind::VerifyFailed, format!("cannot run verification command {program}: {e}")).hint(if cfg!(windows) {
                "on Windows, run batch files through cmd /c"
            } else {
                "check that the command exists and is executable"
            })
        })?;
    if !status.success() {
        let code = status.code();
        let shown = code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string());
        return Err(Error::new(Kind::VerifyFailed, format!("verification command failed (exit {shown})"))
            .hint("fix the installation test or the packages, then rerun ppr-tool verify")
            .detail(json!({ "childExitCode": code })));
    }
    let after = reprepare(&state_value).map_err(|e| Error::state(format!("verification changed release artifacts: {}", e.message)))?;
    if after.digest != prepared.digest {
        return Err(Error::state("verification changed release artifacts"));
    }
    state_value.tested_digest = Some(prepared.digest.clone());
    state::save(path, &state_value)?;
    ui.success(&format!("Verified release {}", state_value.release_id));
    Ok(())
}
