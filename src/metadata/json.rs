use crate::error::{Error, Result};
use serde_json::Value;

pub fn parse(text: &str) -> Result<Value> {
    serde_json::from_str::<Value>(text).map_err(|e| Error::validation(format!("invalid JSON: {e}")))
}
