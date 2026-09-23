//! Package metadata interpretation. The registry parses the same files with JavaScript libraries
//! (fast-xml-parser, smol-toml, JSON.parse) and compares the result with what the tool declared,
//! so these converters must produce identical value shapes.

mod cargo;
mod json;
mod nuspec;

use crate::error::{Error, Result};
use crate::names::Format;
use serde_json::{Map, Value};

pub type Metadata = Map<String, Value>;

pub fn parse_package_metadata(format: Format, text: &str) -> Result<Metadata> {
    let value = match format {
        Format::Npm | Format::Composer => json::parse(text)?,
        Format::Cargo => cargo::parse(text)?,
        Format::Nuget => nuspec::parse(text)?,
    };
    let Value::Object(object) = value else { return Err(Error::validation("package metadata must be an object")) };
    for (key, field) in [("name", identity_field(format, "name")), ("version", identity_field(format, "version"))] {
        let problem = match object.get(key) {
            Some(Value::String(_)) => continue,
            None | Some(Value::Null) => "is missing",
            Some(Value::Object(_)) if format == Format::Nuget => "must contain text only",
            Some(Value::Array(_)) if format == Format::Nuget => "must appear only once",
            Some(_) => "must be a string",
        };
        return Err(Error::validation(format!("{field} {problem}")).hint("the archive must declare its package identity"));
    }
    Ok(object)
}

/// Where the package identity lives in the source file, for error messages.
fn identity_field(format: Format, key: &str) -> String {
    match (format, key) {
        (Format::Cargo, _) => format!("[package].{key}"),
        (Format::Nuget, "name") => "<id>".to_string(),
        (Format::Nuget, _) => format!("<{key}>"),
        _ => format!("\"{key}\""),
    }
}

pub fn string<'a>(metadata: &'a Metadata, key: &str) -> Option<&'a str> {
    metadata.get(key).and_then(Value::as_str)
}
