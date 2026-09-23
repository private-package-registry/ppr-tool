//! Cargo.toml → JSON with the shape smol-toml produces, plus top-level name/version.

use crate::error::{Error, Result};
use serde_json::{Map, Number, Value};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub fn parse(text: &str) -> Result<Value> {
    let document: toml::Value = toml::from_str(text).map_err(|e| Error::validation(format!("invalid Cargo.toml: {}", e.message())))?;
    let mut value = convert(&document)?;
    let Value::Object(object) = &mut value else { return Err(Error::validation("Cargo.toml must be a table")) };
    let Some(package) = object.get("package").and_then(Value::as_object).cloned() else { return Err(Error::validation("missing [package] table")) };
    let name = package.get("name").cloned().unwrap_or(Value::Null);
    let version = package.get("version").cloned().unwrap_or(Value::Null);
    object.insert("name".to_string(), name);
    object.insert("version".to_string(), version);
    Ok(value)
}

fn convert(value: &toml::Value) -> Result<Value> {
    Ok(match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => {
            if i.unsigned_abs() > MAX_SAFE_INTEGER {
                return Err(Error::validation(format!("integer {i} cannot be represented losslessly in JSON")));
            }
            Value::Number(Number::from(*i))
        }
        toml::Value::Float(f) => match Number::from_f64(*f) {
            Some(n) => Value::Number(n),
            None => Value::Null,
        },
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(_) => {
            return Err(Error::validation("datetime values in Cargo.toml are not supported").hint("remove the date or time value from the crate manifest"));
        }
        toml::Value::Array(items) => Value::Array(items.iter().map(convert).collect::<Result<Vec<_>>>()?),
        toml::Value::Table(table) => {
            let mut map = Map::new();
            for (key, item) in table {
                map.insert(key.clone(), convert(item)?);
            }
            Value::Object(map)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hoists_identity() {
        let value = parse("[package]\nname = \"demo\"\nversion = \"1.2.3\"\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n").unwrap();
        assert_eq!(value["name"], "demo");
        assert_eq!(value["version"], "1.2.3");
        assert_eq!(value["dependencies"]["serde"]["features"][0], "derive");
    }

    #[test]
    fn rejects_datetime_and_huge_integers() {
        assert!(parse("[package]\nname=\"a\"\nversion=\"1\"\nd = 1979-05-27\n").is_err());
        assert!(parse("[package]\nname=\"a\"\nversion=\"1\"\nn = 9007199254740993\n").is_err());
        assert!(parse("[package]\nname=\"a\"\nversion=\"1\"\nn = -9223372036854775808\n").is_err());
        assert!(parse("[package]\nname=\"a\"\nversion=\"1\"\nn = -9007199254740991\n").is_ok());
        assert_eq!(parse("[package]\nname=\"a\"\nversion=\"1\"\nf = inf\n").unwrap()["f"], Value::Null);
    }
}
