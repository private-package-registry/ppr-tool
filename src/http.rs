//! Registry HTTP client: origin rules, retries, response limits and no redirects. Archives do not pass
//! through the registry: they are PUT to object storage through presigned URLs on a separate agent
//! that never carries the registry credential.

use crate::error::{Error, Kind, Result};
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

pub const USER_AGENT: &str = concat!("ppr-tool/", env!("CARGO_PKG_VERSION"));
/// Publication API prefix (docs/API.md). v1 was withdrawn together with the Cloudflare Worker registry.
pub const API: &str = "/api/v2";
pub const OIDC_ROUTE: &str = "/api/v2/auth/oidc";
const MAX_RESPONSE: u64 = 4 * 1024 * 1024;
const MAX_ERROR_RESPONSE: u64 = 64 * 1024;
pub const ATTEMPTS: u32 = 4;
const MAX_UPLOAD_HEADERS: usize = 32;
/// Headers a presigned upload must never carry: credentials, cookies and anything that controls framing
/// or routing, which the tool derives from the URL and the file itself.
const FORBIDDEN_UPLOAD_HEADERS: [&str; 12] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "expect",
    "te",
    "trailer",
    "upgrade",
    "keep-alive",
];

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

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Presigned upload URLs must be HTTPS. Plain HTTP is accepted only for loopback storage, and only when
/// `allow_http_loopback` is set (debug builds, or a registry that itself runs on plain-HTTP loopback).
pub fn upload_url_allowed(url: &url::Url, allow_http_loopback: bool) -> bool {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => allow_http_loopback && is_loopback(url),
        _ => false,
    }
}

fn is_token(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Checks the headers the registry asked to send with a presigned PUT. A `Host` equal to the URL's
/// authority is dropped (the URL already determines it); every other forbidden header is refused.
pub fn upload_headers(url: &url::Url, headers: &[(String, String)]) -> Result<Vec<(String, String)>> {
    if headers.len() > MAX_UPLOAD_HEADERS {
        return Err(Error::registry("registry returned too many upload headers"));
    }
    let host = url.host_str().unwrap_or_default();
    let same_authority = |value: &str| {
        let explicit = url.port_or_known_default().map(|port| format!("{host}:{port}"));
        (url.port().is_none() && value.eq_ignore_ascii_case(host)) || explicit.is_some_and(|a| value.eq_ignore_ascii_case(&a))
    };
    let mut out: Vec<(String, String)> = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if !is_token(name) || value.len() > 1024 || !value.bytes().all(|b| b == b'\t' || (0x20..0x7f).contains(&b)) {
            return Err(Error::registry("registry returned an invalid upload header"));
        }
        // A header named twice (in any case) would be sent twice and break the signature.
        if out.iter().any(|(kept, _)| kept.eq_ignore_ascii_case(name)) {
            return Err(Error::registry("registry returned a duplicate upload header"));
        }
        if lower == "host" && same_authority(value) {
            continue;
        }
        if FORBIDDEN_UPLOAD_HEADERS.contains(&lower.as_str()) {
            // Never forward credentials or framing headers to object storage, whatever the registry says.
            return Err(Error::registry(format!("registry asked to send a forbidden {lower} header with the upload"))
                .hint("the registry's upload URL configuration is broken; report it to the registry operator"));
        }
        out.push((name.clone(), value.clone()));
    }
    Ok(out)
}

pub enum Body<'a> {
    None,
    Json(&'a [u8]),
}

struct ProgressReader<'a> {
    /// Limited to the declared size: never send more bytes than the `Content-Length`.
    inner: std::io::Take<std::fs::File>,
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

/// Result of a presigned PUT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    Stored,
    /// A final non-retryable status; 403 usually means the signature expired.
    Rejected(u16),
    /// Every attempt lost the connection (not a timeout). Storage may close the connection instead of
    /// answering 403 when it refuses a large body early, for example on an expired signature.
    Interrupted(String),
}

#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    /// Separate agent for object storage: no registry credential, no shared connections.
    storage: ureq::Agent,
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

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(200 * 2u64.pow(attempt))
}

fn retry_after(response: &ureq::http::Response<ureq::Body>) -> Option<Duration> {
    let seconds = response.headers().get("retry-after")?.to_str().ok()?.trim().parse::<u64>().ok().filter(|s| *s > 0)?;
    Some(Duration::from_secs(seconds.min(10)))
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

/// Keeps only a code from a fixed alphabet, so an error body can never smuggle text into the output.
pub fn safe_code(value: &str) -> Option<&str> {
    let mut bytes = value.bytes();
    let first = bytes.next()?;
    (first.is_ascii_lowercase() && value.len() <= 64 && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')).then_some(value)
}

/// `error.code` of a failed registry response; the rest of the body is never read into messages.
fn error_code(response: &mut ureq::http::Response<ureq::Body>) -> Option<String> {
    let bytes = response.body_mut().with_config().limit(MAX_ERROR_RESPONSE).read_to_vec().ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    safe_code(value.get("error")?.get("code")?.as_str()?).map(str::to_string)
}

fn hint_for(status: u16, code: Option<&str>) -> Option<&'static str> {
    Some(match (status, code) {
        (401 | 403, _) => "the credential is invalid, expired or not permitted for this product",
        (404, _) => "the product, release or artifact does not exist on this registry, or the registry does not implement publication API v2",
        (409, Some("stale_upload")) => "another ppr-tool process is uploading this release; run one publication per release at a time",
        (409, Some("not_draft")) => "the release is no longer a draft; run ppr-tool status",
        (409, Some("incomplete")) => "an archive of the release is not ready on the registry; rerun ppr-tool stage to resume the draft",
        (409, Some("manifest_mismatch")) => "the registry's release does not match the local manifest; run ppr-tool status",
        (409, _) => "this product/version/variant is already reserved with different content",
        (412, _) => "the archive did not arrive in the registry's storage; rerun ppr-tool stage",
        (422, _) => "the registry rejected the package set or archive contents; check the product catalog",
        _ => return None,
    })
}

fn status_error(method: &str, route: &str, status: u16, code: Option<String>) -> Error {
    let kind = if status == 401 || status == 403 { Kind::Auth } else { Kind::Registry };
    let suffix = code.as_deref().map(|c| format!(" ({c})")).unwrap_or_default();
    let mut detail = json!({ "httpStatus": status });
    if let Some(code) = &code {
        detail["code"] = json!(code);
    }
    let mut error = Error::new(kind, format!("registry {method} {route} failed with HTTP {status}{suffix}")).detail(detail);
    error.hint = hint_for(status, code.as_deref()).map(str::to_string);
    error
}

pub const STORAGE_HINT: &str = "check that this machine can reach the registry's object storage, then rerun ppr-tool stage to resume";

impl Client {
    pub fn new(registry: &str) -> Result<Client> {
        Ok(Client { agent: build_agent(), storage: build_agent(), registry: registry_url(registry)?, token: None })
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

    /// Registry request returning the JSON body; see [`Client::call`].
    pub fn request(&self, method: &str, route: &str, body: Body<'_>, headers: &[(&str, &str)]) -> Result<Value> {
        self.call(method, route, body, headers).map(|response| response.json)
    }

    /// Registry request with retries on 429/5xx and transport failures. Every publication request carries
    /// an idempotency key, so repeating one is safe. Error bodies are never printed; only `error.code` is kept.
    pub fn call(&self, method: &str, route: &str, body: Body<'_>, headers: &[(&str, &str)]) -> Result<Response> {
        let url = format!("{}{}", self.registry, route);
        let mut attempt = 0u32;
        loop {
            let result = match method {
                "GET" => self.decorate(self.agent.get(&url), headers).call(),
                "POST" => {
                    let request = self.decorate(self.agent.post(&url), headers);
                    match body {
                        Body::None => request.send_empty(),
                        Body::Json(bytes) => request.header("content-type", "application/json").send(bytes),
                    }
                }
                other => return Err(Error::internal(format!("unsupported method {other}"))),
            };
            match result {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    if (status == 429 || status >= 500) && attempt + 1 < ATTEMPTS {
                        let delay = retry_after(&response).unwrap_or_else(|| backoff(attempt));
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
                        return Err(status_error(method, route, status, error_code(&mut response)));
                    }
                    match read_json(&mut response) {
                        Ok(json) => return Ok(Response { status, json }),
                        Err(ReadFailure::Transport(_)) if attempt + 1 < ATTEMPTS => {
                            std::thread::sleep(backoff(attempt));
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
                            std::thread::sleep(backoff(attempt));
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

    /// Streams an archive to a presigned object-storage URL issued by the registry. The URL is the only
    /// authorisation: the request goes through the storage agent with exactly the signed headers plus
    /// `Content-Length`, never the registry credential and never cookies. 429/5xx and transport failures
    /// are retried on the same URL; any other final status, and connections lost on every attempt, are
    /// returned for the caller to decide (a fresh URL may help); timeouts and exhausted 429/5xx are errors.
    /// Neither the URL nor storage response bodies ever appear in messages (the query is a signature).
    pub fn put_presigned(&self, url: &str, path: &Path, size: u64, headers: &[(String, String)], progress: &dyn Fn(u64)) -> Result<PutOutcome> {
        let parsed = url::Url::parse(url).map_err(|_| Error::registry("registry returned an invalid upload URL"))?;
        // A plain-HTTP loopback registry is a local setup; its storage may be plain-HTTP loopback too.
        let allow_http_loopback = cfg!(debug_assertions) || self.registry.starts_with("http://");
        if !upload_url_allowed(&parsed, allow_http_loopback) {
            return Err(Error::registry("registry returned an insecure upload URL")
                .hint("upload URLs must use HTTPS; plain HTTP is accepted only for loopback storage in local setups"));
        }
        let headers = upload_headers(&parsed, headers)?;
        // Generous for slow links: 64 MiB at 256 KiB/s plus two minutes.
        let timeout = Duration::from_secs(120 + size / (256 * 1024));
        let mut attempt = 0u32;
        loop {
            let file = std::fs::File::open(path).map_err(|e| Error::validation(format!("cannot read archive: {e}")))?;
            let mut reader = ProgressReader { inner: file.take(size), sent: 0, progress };
            let mut request = self.storage.put(parsed.as_str()).config().timeout_global(Some(timeout)).build();
            for (name, value) in &headers {
                request = request.header(name.as_str(), value.as_str());
            }
            // No `Expect: 100-continue`: ureq treats an interim response carrying headers as a final one
            // and would wait forever for a server that is waiting for the body.
            let result = request.header("content-length", size.to_string()).send(ureq::SendBody::from_reader(&mut reader));
            match result {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if (200..300).contains(&status) {
                        return Ok(PutOutcome::Stored);
                    }
                    if (status == 429 || status >= 500) && attempt + 1 < ATTEMPTS {
                        let delay = retry_after(&response).unwrap_or_else(|| backoff(attempt));
                        drop(response);
                        std::thread::sleep(delay);
                        attempt += 1;
                        continue;
                    }
                    if (300..400).contains(&status) {
                        return Err(Error::registry(format!("object storage redirected the upload (HTTP {status}); redirects are not followed"))
                            .detail(json!({ "storageStatus": status })));
                    }
                    if status == 429 || status >= 500 {
                        return Err(Error::registry(format!("object storage failed with HTTP {status} (after {ATTEMPTS} attempts)"))
                            .hint(STORAGE_HINT)
                            .detail(json!({ "storageStatus": status })));
                    }
                    return Ok(PutOutcome::Rejected(status));
                }
                Err(error) => match transport_error(&error) {
                    Some(_) if attempt + 1 < ATTEMPTS => {
                        std::thread::sleep(backoff(attempt));
                        attempt += 1;
                    }
                    Some(message) if !matches!(error, ureq::Error::Timeout(_)) => return Ok(PutOutcome::Interrupted(message)),
                    Some(message) => {
                        return Err(Error::registry(format!("upload to object storage failed: {message} (after {ATTEMPTS} attempts)")).hint(STORAGE_HINT));
                    }
                    // Other ureq errors may quote the URL, which carries the signature.
                    None => return Err(Error::registry("upload to object storage failed").hint(STORAGE_HINT)),
                },
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
    let base = format!("{API}/products/{}/releases", encode(product));
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
        assert_eq!(route_for("demo", Some("01j")), "/api/v2/products/demo/releases/01j");
    }

    #[test]
    fn upload_urls() {
        let allowed = |url: &str, http: bool| upload_url_allowed(&url::Url::parse(url).unwrap(), http);
        assert!(allowed("https://bucket.r2.cloudflarestorage.com/uploads/a?X-Amz-Signature=x", false));
        assert!(allowed("http://127.0.0.1:9000/bucket/uploads/a", true));
        assert!(allowed("http://localhost:9000/bucket/uploads/a", true));
        assert!(allowed("http://[::1]:9000/bucket/uploads/a", true));
        assert!(!allowed("http://127.0.0.1:9000/bucket/uploads/a", false), "plain HTTP needs the loopback allowance");
        assert!(!allowed("http://storage.example.com/bucket/uploads/a", true), "plain HTTP only on loopback");
        assert!(!allowed("http://10.0.0.1/bucket/uploads/a", true));
        assert!(!allowed("https://user:secret@storage.example.com/a", false));
        assert!(!allowed("https://storage.example.com/a#fragment", false));
        assert!(!allowed("ftp://storage.example.com/a", true));
        assert!(!allowed("file:///etc/passwd", true));
    }

    #[test]
    fn upload_header_rules() {
        let url = url::Url::parse("https://storage.example.com:8443/bucket/key?X-Amz-Signature=x").unwrap();
        let pair = |n: &str, v: &str| (n.to_string(), v.to_string());
        let kept = upload_headers(&url, &[pair("Content-Type", "application/octet-stream"), pair("Host", "storage.example.com:8443")]).unwrap();
        assert_eq!(kept, vec![pair("Content-Type", "application/octet-stream")], "a matching Host is dropped");
        for forbidden in ["Authorization", "cookie", "Proxy-Authorization", "Content-Length", "Transfer-Encoding", "Expect"] {
            assert!(upload_headers(&url, &[pair(forbidden, "x")]).is_err(), "{forbidden} is refused");
        }
        assert!(upload_headers(&url, &[pair("Host", "evil.example.com")]).is_err());
        assert!(upload_headers(&url, &[pair("Host", "storage.example.com")]).is_err(), "the port is part of the authority");
        let default_port = url::Url::parse("https://bucket.s3.amazonaws.com/key").unwrap();
        for host in ["bucket.s3.amazonaws.com", "BUCKET.s3.amazonaws.com:443"] {
            assert!(upload_headers(&default_port, &[pair("Host", host)]).unwrap().is_empty(), "{host}");
        }
        assert!(upload_headers(&url, &[pair("X-Amz-Meta", "a\r\nb")]).is_err());
        assert!(upload_headers(&url, &[pair("bad name", "x")]).is_err());
        let many: Vec<(String, String)> = (0..33).map(|i| pair(&format!("x-h{i}"), "v")).collect();
        assert!(upload_headers(&url, &many).is_err());
        assert!(upload_headers(&url, &[pair("Content-Type", "a"), pair("content-type", "b")]).is_err(), "duplicate names are refused");
    }

    #[test]
    fn error_codes() {
        assert_eq!(safe_code("upload_missing"), Some("upload_missing"));
        assert_eq!(safe_code("stale_upload"), Some("stale_upload"));
        assert_eq!(safe_code("Upload"), None);
        assert_eq!(safe_code("token ppr_s_x"), None);
        assert_eq!(safe_code(""), None);
        assert_eq!(safe_code(&"a".repeat(65)), None);
        let error = status_error("POST", "/x", 412, Some("upload_missing".into()));
        assert_eq!(error.http_status(), Some(412));
        assert_eq!(error.code(), Some("upload_missing"));
        assert!(error.message.ends_with("HTTP 412 (upload_missing)"));
        assert_eq!(status_error("POST", "/x", 403, None).kind, Kind::Auth);
        let incomplete = status_error("POST", "/commit", 409, Some("incomplete".into()));
        assert!(incomplete.hint.unwrap().contains("rerun ppr-tool stage"), "commit conflicts do not claim a reserved identity");
    }
}
