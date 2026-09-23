//! Local publication state: a credential-bearing file written atomically with restrictive permissions.

use crate::error::{Error, Result};
use crate::http::registry_url;
use crate::inputs::Scope;
use crate::names;
use crate::time;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub schema_version: u32,
    pub registry: String,
    pub release_id: String,
    pub product: String,
    pub version: String,
    pub variant: String,
    pub commit: String,
    pub digest: String,
    pub archives: Vec<PathBuf>,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tested_digest: Option<String>,
    pub created_at: String,
}

impl State {
    pub fn scope(&self) -> Scope {
        Scope { product: self.product.clone(), variant: self.variant.clone(), commit: self.commit.clone() }
    }

    pub fn expires_in(&self) -> Option<i64> {
        self.expires_at.as_deref().and_then(time::parse_rfc3339).map(|at| at - time::now())
    }

    pub fn tested(&self) -> bool {
        self.tested_digest.as_deref() == Some(self.digest.as_str())
    }
}

pub fn default_path() -> PathBuf {
    std::env::var_os("PPR_STATE").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".ppr-tool/state.json"))
}

pub fn load(file: &Path) -> Result<State> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| Error::state(format!("cannot read publication state {}: {e}", file.display())).hint("run ppr-tool stage first, or pass --state FILE"))?;
    let state: State = serde_json::from_str(&text).map_err(|_| Error::state(format!("{} is not a valid publication state file", file.display())))?;
    let invalid = || Error::state(format!("{} is not a valid publication state file", file.display())).hint("delete it and run ppr-tool stage again");
    if state.schema_version != 2 {
        return Err(Error::state(format!("{} was written by an incompatible ppr-tool version (schema {})", file.display(), state.schema_version))
            .hint("delete it and run ppr-tool stage again"));
    }
    if !names::is_release_id(&state.release_id)
        || !names::is_hex(&state.digest, 64)
        || state.token.is_empty()
        || state.token.contains(['\r', '\n'])
        || state.archives.is_empty()
        || state.archives.iter().any(|a| !a.is_absolute())
    {
        return Err(invalid());
    }
    if state.tested_digest.as_deref().is_some_and(|d| !names::is_hex(d, 64)) {
        return Err(invalid());
    }
    let registry = registry_url(&state.registry).map_err(|_| invalid())?;
    if registry != state.registry || state.preview != format!("{registry}/preview/{}", state.release_id) {
        return Err(invalid());
    }
    if state.expires_at.as_deref().is_some_and(|e| time::parse_rfc3339(e).is_none()) {
        return Err(invalid());
    }
    names::check_product(&state.product).map_err(|_| invalid())?;
    names::check_variant(&state.variant).map_err(|_| invalid())?;
    names::check_commit(&state.commit).map_err(|_| invalid())?;
    Ok(state)
}

pub fn save(file: &Path, state: &State) -> Result<()> {
    let directory = file.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    create_private_dir(&directory)?;
    let unique = format!("{}.{}-{}.tmp", file.file_name().and_then(|n| n.to_str()).unwrap_or("state.json"), std::process::id(), time::now());
    let temporary = directory.join(unique);
    let content = format!("{}\n", serde_json::to_string_pretty(state)?);
    write_private_file(&temporary, content.as_bytes())?;
    if let Err(error) = std::fs::rename(&temporary, file) {
        let _ = std::fs::remove_file(&temporary);
        return Err(Error::internal(format!("cannot write publication state {}: {error}", file.display())));
    }
    Ok(())
}

fn create_private_dir(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(|e| Error::internal(format!("cannot create {}: {e}", directory.display())))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(directory).map_err(|e| Error::internal(format!("cannot create {}: {e}", directory.display())))?;
    }
    Ok(())
}

fn write_private_file(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| Error::internal(format!("cannot create {}: {e}", path.display())))?;
    file.write_all(content).map_err(|e| Error::internal(format!("cannot write {}: {e}", path.display())))?;
    file.sync_all().ok();
    Ok(())
}
