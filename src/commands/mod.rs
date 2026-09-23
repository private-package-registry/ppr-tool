//! Command implementations and the pieces they share.

pub mod commit;
pub mod publish;
pub mod stage;
pub mod status;
pub mod validate;
pub mod verify;

use crate::auth;
use crate::cli::{RegistryArgs, ReleaseArgs, StateArgs};
use crate::error::{Error, Result};
use crate::http::{Client, registry_url};
use crate::inputs::ArchiveInput;
use crate::names::Format;
use crate::output::Ui;
use crate::release::{self, Prepared};
use crate::state::{self, State};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct Context {
    pub ui: Ui,
}

pub fn state_path(args: &StateArgs) -> PathBuf {
    args.state.clone().unwrap_or_else(state::default_path)
}

/// Rebuilds the release from the archives recorded in the state and checks that nothing changed.
pub fn reprepare(state: &State) -> Result<Prepared> {
    let mut inputs = Vec::with_capacity(state.archives.len());
    for path in &state.archives {
        let display = path.to_string_lossy().into_owned();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let format = Format::from_file_name(name).ok_or_else(|| Error::state(format!("{display}: unsupported archive extension in publication state")))?;
        if !path.is_file() {
            return Err(Error::state(format!("{display}: staged archive is missing")).hint("keep the built archives in place until the release is committed"));
        }
        inputs.push(ArchiveInput { path: path.clone(), display, format });
    }
    let prepared = release::prepare(&state.scope(), &inputs, Some(&state.version))?;
    if prepared.digest != state.digest {
        return Err(Error::state("artifacts changed after staging").hint("rerun ppr-tool stage with the current archives, or restore the staged ones"));
    }
    Ok(prepared)
}

/// Loads the state, checks the optional --release/--registry cross-checks and returns a client with a fresh session.
pub fn open_session(ctx: &Context, state_args: &StateArgs, release: &ReleaseArgs, registry: &RegistryArgs) -> Result<(PathBuf, State, Client)> {
    let path = state_path(state_args);
    let mut current = state::load(&path)?;
    ctx.ui.protect(&current.token);
    crate::actions::add_mask(&current.token);
    if let Some(expected) = release.release.as_ref().filter(|r| **r != current.release_id) {
        return Err(Error::state(format!("release {expected} does not match the staged release {}", current.release_id))
            .hint(format!("omit --release or pass --release {}", current.release_id)));
    }
    if let Some(registry) = registry.registry.as_deref().filter(|r| !r.is_empty())
        && registry_url(registry)? != current.registry
    {
        return Err(Error::state(format!("registry {registry} does not match the staged registry {}", current.registry)));
    }
    let mut client = Client::new(&current.registry)?;
    refresh_session(ctx, &path, &mut current, &mut client)?;
    Ok((path, current, client))
}

/// Replaces a session that is about to expire and points the client at the current token.
pub fn refresh_session(ctx: &Context, path: &Path, current: &mut State, client: &mut Client) -> Result<()> {
    if auth::needs_refresh(current.expires_at.as_deref()) {
        ctx.ui.info("Publication session is about to expire; requesting a new one");
        client.set_token(None);
        let session = auth::authenticate(client, &current.scope(), &current.version, &ctx.ui)?;
        current.token = session.token;
        current.expires_at = session.expires_at;
        state::save(path, current)?;
    }
    client.set_token(Some(current.token.clone()));
    Ok(())
}

pub struct ReleaseStatus {
    pub digest: String,
    pub complete: bool,
}

pub fn fetch_status(client: &Client, product: &str, release_id: &str) -> Result<ReleaseStatus> {
    let status = client.request("GET", &crate::http::route_for(product, Some(release_id)), crate::http::Body::None, &[])?;
    Ok(ReleaseStatus {
        digest: status.get("manifestSha256").and_then(Value::as_str).unwrap_or_default().to_string(),
        complete: status.get("complete").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Absolute path of the preview sidecar, so verification commands may change directory.
pub fn preview_path(state_file: &Path) -> PathBuf {
    let file = state_file.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| p.join("preview.json")).unwrap_or_else(|| PathBuf::from("preview.json"));
    std::path::absolute(&file).unwrap_or(file)
}

pub fn print_json(value: &Value) {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

pub fn package_rows(prepared: &Prepared) -> Vec<Vec<String>> {
    let mut rows = vec![vec!["FORMAT".to_string(), "PACKAGE".to_string(), "VERSION".to_string(), "SIZE".to_string(), "FILE".to_string()]];
    for package in &prepared.packages {
        rows.push(vec![
            package.format.to_string(),
            package.name.clone(),
            package.version.clone(),
            crate::output::human_size(package.size),
            package.display.clone(),
        ]);
    }
    rows
}

pub fn package_json(prepared: &Prepared) -> Vec<Value> {
    prepared.packages.iter().map(|p| serde_json::json!({ "format": p.format, "name": p.name, "version": p.version, "key": p.key, "size": p.size, "sha256": p.sha256, "file": p.path.to_string_lossy() })).collect()
}
