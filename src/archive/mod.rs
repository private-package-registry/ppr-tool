//! Archive readers that mirror the registry's server-side rules (registry/app/Publication/Archives),
//! so a package rejected by the server is rejected locally with a readable message.

mod tar;
mod zip;

use crate::error::{Error, Result};
use crate::names::Format;

pub const MAX_ARCHIVE: u64 = 64 * 1024 * 1024;
pub const MAX_METADATA: u64 = 256 * 1024;
pub const MAX_ENTRIES: usize = 10_000;

/// Path rules shared by TAR and ZIP entries.
pub fn safe_path(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains("//")
        && !name.contains(':')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.split('/').any(|segment| segment == ".." || segment == ".")
}

/// The package metadata file found in an archive.
pub struct MetadataFile {
    /// Entry path inside the archive, for example `Demo.nuspec`.
    pub name: String,
    pub text: String,
}

/// Extracts the package metadata file (package.json, composer.json, Cargo.toml or .nuspec) as UTF-8 text.
pub fn metadata_file(format: Format, bytes: &[u8]) -> Result<MetadataFile> {
    let (name, raw) = if format.is_zip() { zip::metadata(format, bytes)? } else { tar::metadata(format, bytes)? };
    let text = std::str::from_utf8(&raw).map_err(|_| Error::validation(format!("{name} is not valid UTF-8")))?;
    Ok(MetadataFile { text: text.strip_prefix('\u{feff}').unwrap_or(text).to_string(), name })
}

/// Entry names are decoded like the registry's TextDecoder (the WHATWG decoder the v1 server used):
/// invalid UTF-8 becomes U+FFFD and one leading byte order mark is dropped, so a name such as
/// BOM + `composer.json` is the metadata file on both sides.
pub(crate) fn decode_name(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_string()
}

/// ISO-8859-1: how the registry (like fflate before it) reads ZIP local names without the UTF-8 flag.
pub(crate) fn decode_latin1(raw: &[u8]) -> String {
    raw.iter().map(|&b| b as char).collect()
}

pub(crate) fn invalid(what: &str) -> Error {
    Error::validation(what.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert!(safe_path("package/package.json"));
        assert!(!safe_path("/etc/passwd"));
        assert!(!safe_path("a//b"));
        assert!(!safe_path("a/../b"));
        assert!(!safe_path("./a"));
        assert!(!safe_path("c:/x"));
        assert!(!safe_path("a\\b"));
        assert!(!safe_path(""));
    }
}
