//! Publication API v2 (docs/API.md): the calls the tool makes and strict parsing of their responses, so
//! an outdated or misbehaving registry fails with a clear message instead of an inconsistent state.

use crate::error::{Error, Result};
use crate::http::{Body, Client, route_for};
use crate::names;
use crate::release::Prepared;
use serde_json::{Value, json};
use std::collections::HashSet;

fn invalid(what: &str) -> Error {
    Error::registry(format!("registry returned an invalid {what}")).hint("the registry does not implement publication API v2 as ppr-tool expects")
}

fn is_key(value: &str) -> bool {
    names::is_hex(value, 64)
}

fn key_set(value: Option<&Value>) -> Option<HashSet<String>> {
    value?.as_array()?.iter().map(|k| k.as_str().filter(|k| is_key(k)).map(str::to_string)).collect()
}

pub struct StagedRelease {
    pub id: String,
    /// Keys the registry already verified; the rest are pending uploads.
    pub uploaded: HashSet<String>,
    pub pending: HashSet<String>,
}

/// Creates or resumes the draft. `uploaded` and `pending` must partition the manifest's keys.
pub fn stage(client: &Client, prepared: &Prepared) -> Result<StagedRelease> {
    let value = client.request("POST", &route_for(&prepared.scope.product, None), Body::Json(&prepared.wire), &[("idempotency-key", &prepared.digest)])?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| names::is_release_id(id))
        .ok_or_else(|| Error::registry("registry returned an invalid release identity"))?
        .to_string();
    if value.get("manifestSha256").and_then(Value::as_str) != Some(prepared.digest.as_str()) {
        return Err(Error::registry("registry returned a different release identity").hint("the registry's manifest digest does not match the local one"));
    }
    let (Some(uploaded), Some(pending)) = (key_set(value.get("uploaded")), key_set(value.get("pending"))) else {
        return Err(invalid("upload list"));
    };
    let keys: HashSet<&str> = prepared.packages.iter().map(|p| p.key.as_str()).collect();
    let consistent = uploaded.is_disjoint(&pending)
        && uploaded.len() + pending.len() == keys.len()
        && uploaded.iter().chain(pending.iter()).all(|k| keys.contains(k.as_str()));
    if !consistent {
        return Err(Error::registry("registry's upload list does not match the manifest").hint("rerun ppr-tool stage; report it if it persists"));
    }
    Ok(StagedRelease { id, uploaded, pending })
}

pub struct ArtifactStatus {
    pub key: String,
    /// `declared`, `uploading`, `verifying`, `ready` or `failed`.
    pub state: String,
}

pub struct ReleaseStatus {
    pub digest: String,
    pub complete: bool,
    pub artifacts: Vec<ArtifactStatus>,
}

impl ReleaseStatus {
    pub fn state_of(&self, key: &str) -> Option<&str> {
        self.artifacts.iter().find(|a| a.key == key).map(|a| a.state.as_str())
    }
    pub fn count(&self, state: &str) -> usize {
        self.artifacts.iter().filter(|a| a.state == state).count()
    }
}

fn is_state(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

pub fn fetch_status(client: &Client, product: &str, release_id: &str) -> Result<ReleaseStatus> {
    let value = client.request("GET", &route_for(product, Some(release_id)), Body::None, &[])?;
    let digest = value.get("manifestSha256").and_then(Value::as_str);
    let complete = value.get("complete").and_then(Value::as_bool);
    let artifacts: Option<Vec<ArtifactStatus>> = value.get("artifacts").and_then(Value::as_array).and_then(|items| {
        items
            .iter()
            .map(|item| {
                let key = item.get("key").and_then(Value::as_str).filter(|k| is_key(k))?;
                let state = item.get("state").and_then(Value::as_str).filter(|s| is_state(s))?;
                Some(ArtifactStatus { key: key.to_string(), state: state.to_string() })
            })
            .collect()
    });
    match (digest, complete, artifacts) {
        (Some(digest), Some(complete), Some(artifacts)) => Ok(ReleaseStatus { digest: digest.to_string(), complete, artifacts }),
        _ => Err(invalid("release status")),
    }
}

pub struct Ticket {
    pub upload_id: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

pub enum Issued {
    /// The artifact is already verified; nothing to upload.
    Ready,
    Upload(Ticket),
}

pub fn artifact_route(product: &str, release_id: &str, key: &str) -> String {
    format!("{}/artifacts/{key}", route_for(product, Some(release_id)))
}

/// Asks for a presigned upload URL (or learns that the artifact is already verified).
pub fn request_upload(client: &Client, route: &str, idempotency: &str) -> Result<Issued> {
    let value = client.request("POST", &format!("{route}/upload-url"), Body::Json(b"{}"), &[("idempotency-key", idempotency)])?;
    parse_ticket(&value)
}

fn parse_ticket(value: &Value) -> Result<Issued> {
    match value.get("ready").and_then(Value::as_bool) {
        Some(true) => return Ok(Issued::Ready),
        Some(false) => {}
        None => return Err(invalid("upload ticket")),
    }
    let upload_id = value.get("uploadId").and_then(Value::as_str).filter(|id| id.len() <= 128 && names::is_release_id(id));
    let url = value.get("url").and_then(Value::as_str).filter(|u| !u.is_empty());
    if value.get("method").and_then(Value::as_str) != Some("PUT") {
        return Err(invalid("upload method"));
    }
    let headers: Option<Vec<(String, String)>> = match value.get("headers") {
        None | Some(Value::Null) => Some(Vec::new()),
        // PHP's json_encode() turns an empty header map into `[]`.
        Some(Value::Array(items)) if items.is_empty() => Some(Vec::new()),
        Some(Value::Object(map)) => map.iter().map(|(name, value)| value.as_str().map(|v| (name.clone(), v.to_string()))).collect(),
        Some(_) => None,
    };
    match (upload_id, url, headers) {
        (Some(upload_id), Some(url), Some(headers)) => Ok(Issued::Upload(Ticket { upload_id: upload_id.to_string(), url: url.to_string(), headers })),
        _ => Err(invalid("upload ticket")),
    }
}

pub enum Completion {
    Ready,
    /// Accepted for asynchronous verification (HTTP 202); poll the release status.
    Verifying,
}

/// Asks the registry to verify the uploaded object. Error statuses (412, 409, 422) surface as errors
/// carrying `httpStatus` and `code`.
pub fn complete_upload(client: &Client, route: &str, idempotency: &str, upload_id: &str) -> Result<Completion> {
    let body = serde_json::to_vec(&json!({ "uploadId": upload_id }))?;
    let response = client.call("POST", &format!("{route}/complete"), Body::Json(&body), &[("idempotency-key", idempotency)])?;
    if response.json.get("ready").and_then(Value::as_bool) == Some(true) {
        return Ok(Completion::Ready);
    }
    if response.status == 202 || response.json.get("state").and_then(Value::as_str) == Some("verifying") {
        return Ok(Completion::Verifying);
    }
    Err(invalid("upload completion"))
}

pub fn commit(client: &Client, product: &str, release_id: &str, digest: &str) -> Result<()> {
    let body = serde_json::to_vec(&json!({ "manifestSha256": digest }))?;
    let idempotency = format!("commit:{digest}");
    client.request("POST", &format!("{}/commit", route_for(product, Some(release_id))), Body::Json(&body), &[("idempotency-key", &idempotency)])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(headers: Value) -> Result<Issued> {
        parse_ticket(
            &json!({ "ready": false, "uploadId": "01k5z9aaaaaaaaaaaaaaaaaaaa", "url": "https://s.example.com/u?X-Amz-Signature=x", "method": "PUT", "headers": headers, "expiresAt": "2026-09-25T12:30:00.000Z" }),
        )
    }

    #[test]
    fn upload_tickets() {
        assert!(matches!(parse_ticket(&json!({ "ready": true })), Ok(Issued::Ready)));
        let Ok(Issued::Upload(parsed)) = ticket(json!({ "Content-Type": "application/octet-stream" })) else { panic!("ticket") };
        assert_eq!(parsed.headers, vec![("Content-Type".to_string(), "application/octet-stream".to_string())]);
        assert!(matches!(ticket(json!([])), Ok(Issued::Upload(t)) if t.headers.is_empty()), "an empty PHP array is an empty map");
        assert!(ticket(json!(["Content-Type: x"])).is_err());
        assert!(ticket(json!({ "Content-Type": ["application/octet-stream"] })).is_err(), "value lists must be joined by the server");
        assert!(parse_ticket(&json!({ "ready": false, "uploadId": "x", "url": "https://s.example.com/u", "method": "POST" })).is_err());
        assert!(parse_ticket(&json!({ "ready": false, "uploadId": "../x", "url": "https://s.example.com/u", "method": "PUT" })).is_err());
        assert!(parse_ticket(&json!({ "ready": false, "uploadId": "x", "url": "", "method": "PUT" })).is_err());
        assert!(parse_ticket(&json!({})).is_err());
    }
}
