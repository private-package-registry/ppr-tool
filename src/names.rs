//! Package formats, name rules and identity helpers (mirrors registry/server/shared/manifest.ts).

use crate::error::{Error, Result};
use std::fmt;

/// Longest package name the registry accepts (registry/server/schemas.ts).
pub const MAX_NAME: usize = 214;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Npm,
    Nuget,
    Composer,
    Cargo,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Npm, Format::Nuget, Format::Composer, Format::Cargo];

    pub fn as_str(self) -> &'static str {
        match self {
            Format::Npm => "npm",
            Format::Nuget => "nuget",
            Format::Composer => "composer",
            Format::Cargo => "cargo",
        }
    }

    pub fn parse(value: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.as_str() == value)
    }

    /// Format inferred from the archive file name.
    pub fn from_file_name(name: &str) -> Option<Format> {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".tgz") || lower.ends_with(".tar.gz") {
            Some(Format::Npm)
        } else if lower.ends_with(".crate") {
            Some(Format::Cargo)
        } else if lower.ends_with(".nupkg") {
            Some(Format::Nuget)
        } else if lower.ends_with(".zip") {
            Some(Format::Composer)
        } else {
            None
        }
    }

    pub const EXTENSIONS: &'static str = ".tgz/.tar.gz (npm), .crate (cargo), .nupkg (nuget), .zip (composer)";

    pub fn is_zip(self) -> bool {
        matches!(self, Format::Composer | Format::Nuget)
    }

    /// Path of the metadata file inside the archive, or a matcher for formats where the directory name varies.
    pub fn is_metadata_path(self, name: &str) -> bool {
        match self {
            Format::Npm => name == "package/package.json",
            Format::Composer => name == "composer.json",
            Format::Cargo => {
                let mut parts = name.splitn(2, '/');
                let first = parts.next().unwrap_or("");
                !first.is_empty() && parts.next() == Some("Cargo.toml")
            }
            Format::Nuget => !name.contains('/') && name.to_ascii_lowercase().ends_with(".nuspec") && name.len() > ".nuspec".len(),
        }
    }

    pub fn metadata_description(self) -> &'static str {
        match self {
            Format::Npm => "package/package.json",
            Format::Composer => "composer.json",
            Format::Cargo => "<crate>/Cargo.toml",
            Format::Nuget => "<Id>.nuspec",
        }
    }

    /// Validates a package name against the per-format pattern.
    pub fn valid_name(self, name: &str) -> bool {
        match self {
            Format::Npm => {
                // ^(?:@[a-z0-9][a-z0-9._-]*\/)?[a-z0-9][a-z0-9._-]*$
                let rest = if let Some(stripped) = name.strip_prefix('@') {
                    let Some((scope, rest)) = stripped.split_once('/') else { return false };
                    if !segment(
                        scope,
                        |c| c.is_ascii_lowercase() || c.is_ascii_digit(),
                        |c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'),
                    ) {
                        return false;
                    }
                    rest
                } else {
                    name
                };
                segment(
                    rest,
                    |c| c.is_ascii_lowercase() || c.is_ascii_digit(),
                    |c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'),
                )
            }
            Format::Composer => {
                // ^[a-z0-9][a-z0-9_.-]*\/[a-z0-9][a-z0-9_.-]*$
                let Some((vendor, package)) = name.split_once('/') else { return false };
                let ok = |s: &str| {
                    segment(
                        s,
                        |c| c.is_ascii_lowercase() || c.is_ascii_digit(),
                        |c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'),
                    )
                };
                ok(vendor) && ok(package)
            }
            // ^[A-Za-z0-9][A-Za-z0-9_.-]*$
            Format::Nuget => segment(name, |c| c.is_ascii_alphanumeric(), |c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
            // ^[A-Za-z][A-Za-z0-9_-]*$
            Format::Cargo => segment(name, |c| c.is_ascii_alphabetic(), |c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')),
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn segment(value: &str, first: impl Fn(char) -> bool, rest: impl Fn(char) -> bool) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if first(c) => chars.all(rest),
        _ => false,
    }
}

/// Canonical name used in identities: NuGet is case-insensitive.
pub fn format_name(format: Format, name: &str) -> String {
    match format {
        Format::Nuget => name.to_lowercase(),
        _ => name.to_string(),
    }
}

/// ^[a-z0-9][a-z0-9-]{0,max-1}$
pub fn is_slug(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && segment(value, |c| c.is_ascii_lowercase() || c.is_ascii_digit(), |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// ^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$
pub fn is_release_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && (*p == "0" || !p.starts_with('0')))
}

pub fn is_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn is_release_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub fn check_product(value: &str) -> Result<()> {
    if is_slug(value, 64) {
        Ok(())
    } else {
        Err(Error::usage(format!("invalid product \"{value}\"")).hint("use lowercase letters, digits and dashes, up to 64 characters"))
    }
}

pub fn check_variant(value: &str) -> Result<()> {
    if is_slug(value, 32) {
        Ok(())
    } else {
        Err(Error::usage(format!("invalid variant \"{value}\"")).hint("use lowercase letters, digits and dashes, up to 32 characters"))
    }
}

pub fn check_commit(value: &str) -> Result<()> {
    if is_commit(value) {
        Ok(())
    } else {
        Err(Error::usage(format!("invalid commit \"{value}\"")).hint("pass the full 40-character lowercase hexadecimal commit SHA"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(Format::Npm.valid_name("@scope/pkg-name.x"));
        assert!(Format::Npm.valid_name("plain"));
        assert!(!Format::Npm.valid_name("@Scope/x"));
        assert!(!Format::Npm.valid_name("@scope/"));
        assert!(Format::Composer.valid_name("org/demo"));
        assert!(!Format::Composer.valid_name("demo"));
        assert!(Format::Nuget.valid_name("Demo.Pkg_1"));
        assert!(!Format::Nuget.valid_name(".x"));
        assert!(Format::Cargo.valid_name("demo_crate-1"));
        assert!(!Format::Cargo.valid_name("1demo"));
    }

    #[test]
    fn versions() {
        assert!(is_release_version("1.2.3"));
        assert!(is_release_version("0.0.0"));
        assert!(!is_release_version("01.2.3"));
        assert!(!is_release_version("1.2"));
        assert!(!is_release_version("1.2.3-beta"));
    }

    #[test]
    fn extensions() {
        assert_eq!(Format::from_file_name("a/b.TGZ"), Some(Format::Npm));
        assert_eq!(Format::from_file_name("x.tar.gz"), Some(Format::Npm));
        assert_eq!(Format::from_file_name("x.crate"), Some(Format::Cargo));
        assert_eq!(Format::from_file_name("x.nupkg"), Some(Format::Nuget));
        assert_eq!(Format::from_file_name("x.zip"), Some(Format::Composer));
        assert_eq!(Format::from_file_name("x.tar"), None);
    }

    #[test]
    fn metadata_paths() {
        assert!(Format::Cargo.is_metadata_path("demo-1.0.0/Cargo.toml"));
        assert!(!Format::Cargo.is_metadata_path("Cargo.toml"));
        assert!(!Format::Cargo.is_metadata_path("a/b/Cargo.toml"));
        assert!(Format::Nuget.is_metadata_path("Demo.NUSPEC"));
        assert!(!Format::Nuget.is_metadata_path("x/Demo.nuspec"));
    }
}
