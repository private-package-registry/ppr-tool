//! Registry HTTP client: origin rules, retries, response limits and no redirects.

use crate::error::{Error, Kind, Result};
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

pub const USER_AGENT: &str = concat!("ppr-tool/", env!("CARGO_PKG_VERSION"));
const MAX_RESPONSE: u64 = 4 * 1024 * 1024;
const ATTEMPTS: u32 = 4;

/// Normalises a registry origin: HTTPS (or HTTP on loopback), no credentials, path, query or fragment.
pub fn registry_url(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Error::usage("registry origin is required").hint("pass --registry https://registry.example.com or set PPR_REGISTRY"));
    }
    let url = url::Url::parse(trimmed).map_err(|e| Error::usage(format!("invalid registry URL \"{trimmed}\": {e}")))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.path().is_empty() || url.path() == "/")
    {
        return Err(Error::usage("registry must be an origin without credentials, path or query").hint("use the form https://registry.example.com"));
    }
    let loopback = matches!(url.host_str(), Some("localhost") | Some("127.0.0.1") | Some("[::1]"));
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback)) {
        return Err(Error::usage("registry requires HTTPS"));
    }
    Ok(url.origin().ascii_serialization())
}

pub enum Body<'a> {
    None,
    Json(&'a [u8]),
    /// A file streamed from disk with an explicit length; reopened on every attempt.
    /// The callback receives the bytes sent so far in the current attempt.
    File {
        path: &'a Path,
        size: u64,
        progress: &'a dyn Fn(u64),
    },
}

struct ProgressReader<'a> {
    inner: std::fs::File,
    sent: u64,
    progress: &'a dyn Fn(u64),
}

impl Read for ProgressReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buffer)?;
        self.sent += n as u64;
        (self.progress)(self.sent);
        Ok(n)
    }
}

enum ReadFailure {
    /// The connection broke while reading; worth retrying.
    Transport(String),
    Fatal(Error),
}

pub struct Response {
    pub status: u16,
    pub json: Value,
}

#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    pub registry: String,
    token: Option<String>,
}

fn build_agent() -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .timeout_global(Some(Duration::from_secs(120)))
        .user_agent(USER_AGENT)
        .tls_config(tls)
        .build();
    ureq::Agent::new_with_config(config)
}

fn transport_error(error: &ureq::Error) -> Option<String> {
    match error {
        ureq::Error::Timeout(_) => Some("request timed out".to_string()),
        ureq::Error::Io(e) => Some(format!("network error: {e}")),
        ureq::Error::ConnectionFailed => Some("connection failed".to_string()),
        ureq::Error::HostNotFound => Some("host not found".to_string()),
        ureq::Error::Tls(reason) => Some(format!("TLS error: {reason}")),
        ureq::Error::Rustls(e) => Some(format!("TLS error: {e}")),
        ureq::Error::BodyStalled => Some("connection stalled".to_string()),
        _ => None,
    }
}

fn read_json(response: &mut ureq::http::Response<ureq::Body>) -> std::result::Result<Value, ReadFailure> {
    if response.status().as_u16() == 204 {
        return Ok(Value::Object(Default::default()));
    }
    let bytes = response.body_mut().with_config().limit(MAX_RESPONSE).read_to_vec().map_err(|e| match e {
        ureq::Error::BodyExceedsLimit(_) => ReadFailure::Fatal(Error::registry("oversized registry response")),
        other => match transport_error(&other) {
            Some(message) => ReadFailure::Transport(message),
            None => ReadFailure::Fatal(Error::registry(format!("cannot read registry response: {other}"))),
        },
    })?;
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_slice(&bytes).map_err(|_| ReadFailure::Fatal(Error::registry("registry returned malformed JSON")))
}

fn read_json_once(response: &mut ureq::http::Response<ureq::Body>) -> Result<Value> {
    read_json(response).map_err(|failure| match failure {
        ReadFailure::Transport(message) => Error::registry(format!("cannot read response: {message}")),
        ReadFailure::Fatal(error) => error,
    })
}

impl Client {
    pub fn new(registry: &str) -> Result<Client> {
        Ok(Client { agent: build_agent(), registry: registry_url(registry)?, token: None })
    }

    pub fn set_token(&mut self, token: Option<String>) {
        self.token = token;
    }

    /// GET on an arbitrary HTTPS URL with a bearer token (used for the GitHub OIDC endpoint). No retries.
    pub fn fetch_json(&self, url: &str, bearer: &str, timeout: Duration) -> Result<Response> {
        let request = self
            .agent
            .get(url)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("authorization", format!("Bearer {bearer}"))
            .header("accept", "application/json");
        let mut response = request.call().map_err(|e| Error::registry(transport_error(&e).unwrap_or_else(|| format!("request failed: {e}"))))?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            return Err(Error::registry("unexpected redirect from the OIDC endpoint"));
        }
        let json = if status == 200 { read_json_once(&mut response)? } else { Value::Null };
        Ok(Response { status, json })
    }

    /// Registry request with retries on 429/5xx and transport failures. Never returns response bodies in errors.
    pub fn request(&self, method: &str, route: &str, body: Body<'_>, headers: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{}{}", self.registry, route);
        let mut attempt = 0u32;
        loop {
            let builder = match method {
                "GET" => Some(self.agent.get(&url)),
                _ => None,
            };
            let result = if let Some(mut get) = builder {
                get = self.decorate(get, headers);
                get.call()
            } else {
                let mut req = match method {
                    "POST" => self.agent.post(&url),
                    "PUT" => self.agent.put(&url),
                    other => return Err(Error::internal(format!("unsupported method {other}"))),
                };
                req = self.decorate(req, headers);
                match body {
                    Body::None => req.send_empty(),
                    Body::Json(bytes) => req.header("content-type", "application/json").send(bytes),
                    Body::File { path, size, progress } => {
                        let file = std::fs::File::open(path).map_err(|e| Error::validation(format!("{}: cannot read archive: {e}", path.display())))?;
                        let mut reader = ProgressReader { inner: file, sent: 0, progress };
                        req.header("content-type", "application/octet-stream")
                            .header("content-length", size.to_string())
                            .send(ureq::SendBody::from_reader(&mut reader))
                    }
                }
            };
            let retry_delay = |attempt: u32| Duration::from_millis(200 * 2u64.pow(attempt));
            match result {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    if (status == 429 || status >= 500) && attempt + 1 < ATTEMPTS {
                        let retry_after =
                            response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok()).filter(|s| *s > 0);
                        let delay = retry_after.map(|s| Duration::from_secs(s.min(10))).unwrap_or_else(|| retry_delay(attempt));
                        drop(response);
                        std::thread::sleep(delay);
                        attempt += 1;
                        continue;
                    }
                    if (300..400).contains(&status) {
                        return Err(Error::registry(format!("registry {method} {route} redirected (HTTP {status}); redirects are not followed"))
                            .hint("check the registry origin"));
                    }
                    if !(200..300).contains(&status) {
                        // Do not print untrusted response bodies; they could echo tokens or signed URLs.
                        let kind = if status == 401 || status == 403 { Kind::Auth } else { Kind::Registry };
                        let mut error = Error::new(kind, format!("registry {method} {route} failed with HTTP {status}"))
                            .detail(serde_json::json!({ "httpStatus": status }));
                        error.hint = match status {
                            401 | 403 => Some("the credential is invalid, expired or not permitted for this product".to_string()),
                            404 => Some("the product or release does not exist on this registry".to_string()),
                            409 => Some("this product/version/variant is already reserved with different content".to_string()),
                            422 => Some("the registry rejected the package set or archive contents; check the product catalog".to_string()),
                            _ => None,
                        };
                        return Err(error);
                    }
                    match read_json(&mut response) {
                        Ok(value) => return Ok(value),
                        Err(ReadFailure::Transport(_)) if attempt + 1 < ATTEMPTS => {
                            // Requests carry idempotency keys, so repeating one is safe.
                            std::thread::sleep(retry_delay(attempt));
                            attempt += 1;
                            continue;
                        }
                        Err(ReadFailure::Transport(message)) => {
                            return Err(Error::registry(format!("registry {method} {route}: {message} (after {ATTEMPTS} attempts)")));
                        }
                        Err(ReadFailure::Fatal(error)) => return Err(error),
                    }
                }
                Err(error) => {
                    if let Some(message) = transport_error(&error) {
                        if attempt + 1 < ATTEMPTS {
                            std::thread::sleep(retry_delay(attempt));
                            attempt += 1;
                            continue;
                        }
                        return Err(Error::registry(format!("registry {method} {route}: {message} (after {ATTEMPTS} attempts)")));
                    }
                    return Err(Error::registry(format!("registry {method} {route}: {error}")));
                }
            }
        }
    }

    fn decorate<B>(&self, mut request: ureq::RequestBuilder<B>, headers: &[(&str, &str)]) -> ureq::RequestBuilder<B> {
        if let Some(token) = &self.token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        request = request.header("accept", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request
    }
}

pub fn route_for(product: &str, id: Option<&str>) -> String {
    let base = format!("/api/v1/products/{}/releases", encode(product));
    match id {
        Some(id) => format!("{base}/{}", encode(id)),
        None => base,
    }
}

/// encodeURIComponent-compatible encoding.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert_eq!(registry_url("https://registry.example.com/").unwrap(), "https://registry.example.com");
        assert_eq!(registry_url("https://registry.example.com:8443").unwrap(), "https://registry.example.com:8443");
        assert_eq!(registry_url("http://127.0.0.1:8787").unwrap(), "http://127.0.0.1:8787");
        assert!(registry_url("http://registry.example.com").is_err());
        assert!(registry_url("https://user:pw@registry.example.com").is_err());
        assert!(registry_url("https://registry.example.com/path").is_err());
        assert!(registry_url("https://registry.example.com/?x=1").is_err());
        assert!(registry_url("").is_err());
    }

    #[test]
    fn encoding() {
        assert_eq!(encode("@scope/name"), "%40scope%2Fname");
        assert_eq!(encode("plain-1.0_x"), "plain-1.0_x");
    }
}
