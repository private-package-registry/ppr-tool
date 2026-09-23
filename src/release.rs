//! Builds the wire manifest from archives: metadata, hashes, identities, canonical bytes and digest.

use crate::archive::{self, MAX_ARCHIVE};
use crate::canonical::{canonical_bytes, sha1_hex, sha256_hex, sha512_base64};
use crate::error::{Error, Result};
use crate::inputs::{ArchiveInput, Scope};
use crate::metadata::{self, Metadata};
use crate::names::{self, Format};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;

pub const MAX_WIRE: usize = 4 * 1024 * 1024;
pub const MAX_METADATA_JSON: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct Package {
    pub format: Format,
    pub name: String,
    pub version: String,
    pub key: String,
    pub size: u64,
    pub sha256: String,
    pub sha512: String,
    pub sha1: String,
    pub metadata: Metadata,
    pub path: PathBuf,
    pub display: String,
}

#[derive(Debug, Clone)]
pub struct Prepared {
    pub scope: Scope,
    pub version: String,
    /// Sorted by key, the order used on the wire.
    pub packages: Vec<Package>,
    pub wire: Vec<u8>,
    pub digest: String,
}

impl Prepared {
    pub fn preview_packages(&self) -> Vec<Value> {
        self.packages.iter().map(|p| json!({ "format": p.format, "name": p.name, "version": p.version })).collect()
    }
}

fn inspect(input: &ArchiveInput) -> Result<Package> {
    let located = |e: Error| e.at(&input.display);
    let bytes = std::fs::read(&input.path).map_err(|e| Error::validation(format!("cannot read archive: {e}"))).map_err(located)?;
    if bytes.is_empty() {
        return Err(located(Error::validation("archive is empty")));
    }
    if bytes.len() as u64 > MAX_ARCHIVE {
        return Err(located(Error::validation("archive exceeds 64 MiB")));
    }
    let file = archive::metadata_file(input.format, &bytes).map_err(located)?;
    let metadata = metadata::parse_package_metadata(input.format, &file.text).map_err(|e| e.at(format!("{}: {}", input.display, file.name)))?;
    let name = metadata::string(&metadata, "name").unwrap_or_default().to_string();
    let version = metadata::string(&metadata, "version").unwrap_or_default().to_string();
    if name.chars().count() > names::MAX_NAME || !input.format.valid_name(&name) {
        return Err(located(Error::validation(format!("package name \"{name}\" is not valid for {}", input.format)).hint(format!(
            "{} names must match the registry's naming rules and be at most {} characters",
            input.format,
            names::MAX_NAME
        ))));
    }
    if !names::is_release_version(&version) {
        return Err(located(
            Error::validation(format!("package version \"{version}\" is not a plain X.Y.Z release version"))
                .hint("every archive must carry the release's stable version"),
        ));
    }
    if metadata.get("private") == Some(&Value::Bool(true)) {
        return Err(located(Error::validation(format!("{name} is marked private and cannot be published"))));
    }
    if input.format == Format::Npm {
        for section in ["dependencies", "optionalDependencies", "peerDependencies"] {
            if let Some(Value::Object(entries)) = metadata.get(section) {
                for (dependency, requirement) in entries {
                    let ok = matches!(requirement, Value::String(s) if !(s.starts_with("workspace:") || s.starts_with("file:") || s.starts_with("link:")));
                    if !ok {
                        return Err(located(
                            Error::validation(format!("unresolved local dependency {dependency} in {section} of {name}"))
                                .hint("publish only packages whose dependencies resolve to registry versions"),
                        ));
                    }
                }
            }
        }
    }
    let metadata_value = Value::Object(metadata.clone());
    if canonical_bytes(&metadata_value).len() > MAX_METADATA_JSON {
        return Err(located(Error::validation("package metadata exceeds 256 KiB")));
    }
    let identity = format!("{}:{}:{}", input.format, names::format_name(input.format, &name), version);
    Ok(Package {
        format: input.format,
        key: sha256_hex(identity.as_bytes()),
        size: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
        sha512: sha512_base64(&bytes),
        sha1: sha1_hex(&bytes),
        name,
        version,
        metadata,
        path: input.path.clone(),
        display: input.display.clone(),
    })
}

/// Reads every archive and produces the wire manifest. `expected_version` is an optional cross-check.
pub fn prepare(scope: &Scope, inputs: &[ArchiveInput], expected_version: Option<&str>) -> Result<Prepared> {
    if inputs.is_empty() {
        return Err(Error::usage("no archives given"));
    }
    if inputs.len() > 256 {
        return Err(Error::validation("a release may contain at most 256 packages"));
    }
    let mut packages = Vec::with_capacity(inputs.len());
    let mut identities: HashSet<String> = HashSet::new();
    for input in inputs {
        let package = inspect(input)?;
        let identity = format!("{}:{}", package.format, names::format_name(package.format, &package.name));
        if !identities.insert(identity.clone()) {
            return Err(Error::validation(format!("{}: duplicate package {identity}", input.display)).hint("each package may appear once per release"));
        }
        packages.push(package);
    }
    let version = packages[0].version.clone();
    if packages.iter().any(|p| p.version != version) {
        let mut lines: Vec<String> = packages.iter().map(|p| format!("  {} → {}", p.display, p.version)).collect();
        lines.sort();
        return Err(Error::validation(format!("archives disagree on the release version:\n{}", lines.join("\n")))
            .hint("build every package with the same X.Y.Z version"));
    }
    if let Some(expected) = expected_version.filter(|e| *e != version) {
        return Err(Error::validation(format!("archives carry version {version} but --version {expected} was requested")));
    }
    packages.sort_by(|a, b| a.key.cmp(&b.key));
    let wire_value = json!({
        "schemaVersion": 1,
        "product": scope.product,
        "version": version,
        "variant": scope.variant,
        "commit": scope.commit,
        "packages": packages.iter().map(|p| json!({
            "format": p.format,
            "name": p.name,
            "version": p.version,
            "key": p.key,
            "size": p.size,
            "sha256": p.sha256,
            "sha512": p.sha512,
            "sha1": p.sha1,
            "metadata": Value::Object(p.metadata.clone()),
        })).collect::<Vec<_>>(),
    });
    let wire = canonical_bytes(&wire_value);
    if wire.len() > MAX_WIRE {
        return Err(Error::validation("release manifest exceeds 4 MiB").hint("reduce package metadata size"));
    }
    let digest = sha256_hex(&wire);
    Ok(Prepared { scope: scope.clone(), version, packages, wire, digest })
}
