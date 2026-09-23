//! Positional archive arguments (files and globs) and the release scope flags.

use crate::error::{Error, Result};
use crate::names::{self, Format};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ArchiveInput {
    /// Absolute, canonical path.
    pub path: PathBuf,
    /// The path as the user wrote it (for messages and tables).
    pub display: String,
    pub format: Format,
}

fn has_glob_characters(value: &str) -> bool {
    value.contains(['*', '?', '['])
}

fn classify(path: &Path, display: &str) -> Result<ArchiveInput> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let Some(format) = Format::from_file_name(name) else {
        return Err(Error::validation(format!("{display}: unsupported archive extension")).hint(format!("accepted extensions: {}", Format::EXTENSIONS)));
    };
    let canonical = std::fs::canonicalize(path).map_err(|e| Error::validation(format!("{display}: {e}")))?;
    if !canonical.is_file() {
        return Err(Error::validation(format!("{display}: not a regular file")));
    }
    Ok(ArchiveInput { path: canonical, display: display.to_string(), format })
}

/// Expands the archive arguments. Literal paths win; otherwise glob patterns are expanded here
/// because Windows shells do not expand them. Zero matches are an error; a file matched by several
/// arguments (overlapping globs) is used once.
pub fn expand(arguments: &[String]) -> Result<Vec<ArchiveInput>> {
    if arguments.is_empty() {
        return Err(Error::usage("no archives given").hint("pass archive files or glob patterns, for example dist/npm/*.tgz dist/nuget/*.nupkg"));
    }
    let mut inputs = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for argument in arguments {
        let literal = Path::new(argument);
        let mut matches: Vec<(PathBuf, String)> = Vec::new();
        if literal.exists() {
            matches.push((literal.to_path_buf(), argument.clone()));
        } else if has_glob_characters(argument) {
            let paths = glob::glob(argument).map_err(|e| Error::usage(format!("invalid pattern \"{argument}\": {}", e.msg)))?;
            for entry in paths {
                let path = entry.map_err(|e| Error::validation(format!("{argument}: {}", e.error())))?;
                if path.is_file() {
                    let display = path.to_string_lossy().into_owned();
                    matches.push((path, display));
                }
            }
            matches.sort();
            if matches.is_empty() {
                return Err(Error::validation(format!("no files match \"{argument}\"")));
            }
        } else {
            return Err(Error::validation(format!("{argument}: file not found")));
        }
        for (path, display) in matches {
            let input = classify(&path, &display)?;
            if seen.insert(input.path.clone()) {
                inputs.push(input);
            }
        }
    }
    Ok(inputs)
}

/// Release identity supplied by flags or environment.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Scope {
    pub product: String,
    pub variant: String,
    pub commit: String,
}

impl Scope {
    pub fn resolve(product: Option<String>, variant: Option<String>, commit: Option<String>) -> Result<Scope> {
        let product = product.ok_or_else(|| Error::usage("--product is required").hint("pass --product NAME or set PPR_PRODUCT"))?;
        let variant = variant.ok_or_else(|| Error::usage("--variant is required").hint("pass --variant NAME or set PPR_VARIANT"))?;
        let commit = match commit {
            Some(value) => value,
            None if crate::actions::active() => std::env::var("GITHUB_SHA")
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Error::usage("--commit is required").hint("GITHUB_SHA is not set; pass --commit SHA or set PPR_COMMIT"))?,
            None => {
                return Err(
                    Error::usage("--commit is required").hint("pass --commit SHA or set PPR_COMMIT; on GitHub Actions GITHUB_SHA is used automatically")
                );
            }
        };
        names::check_product(&product)?;
        names::check_variant(&variant)?;
        names::check_commit(&commit)?;
        Ok(Scope { product, variant, commit })
    }
}
