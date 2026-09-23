//! Shared helpers for the integration tests: archive builders, a scratch directory, a runner for the
//! built binary and a mock registry that implements the publication protocol.
#![allow(dead_code)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------------------------
// Scratch directories

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = format!("ppr-tool-test-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst));
        let path = std::env::temp_dir().join(unique);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path.canonicalize().unwrap())
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    pub fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------------------------
// Archives

/// Uncompressed ustar stream. `kind` is the typeflag byte (b'0' file, b'5' directory, b'2' symlink).
pub fn tar(entries: &[(&str, &[u8], u8)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data, kind) in entries {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = *kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        out.resize(out.len() + (512 - data.len() % 512) % 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

pub fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let with_kind: Vec<(&str, &[u8], u8)> = entries.iter().map(|(n, d)| (*n, *d, b'0')).collect();
    gzip(&tar(&with_kind))
}

#[derive(Clone)]
pub struct ZipEntry<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
    pub deflate: bool,
    /// Write sizes and CRC in a trailing data descriptor instead of the local header.
    pub descriptor: bool,
}

impl<'a> ZipEntry<'a> {
    pub fn new(name: &'a str, data: &'a [u8]) -> Self {
        ZipEntry { name, data, deflate: true, descriptor: false }
    }
}

/// Minimal ZIP writer (no zip64, no extra fields).
pub fn zip(entries: &[ZipEntry<'_>]) -> Vec<u8> {
    zip_with(entries, &[])
}

/// ZIP writer that can insert raw bytes before the central directory (to simulate hidden entries).
pub fn zip_with(entries: &[ZipEntry<'_>], before_directory: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        let mut crc = flate2::Crc::new();
        crc.update(entry.data);
        let crc = crc.sum();
        let payload = if entry.deflate {
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(entry.data).unwrap();
            encoder.finish().unwrap()
        } else {
            entry.data.to_vec()
        };
        let method: u16 = if entry.deflate { 8 } else { 0 };
        let flags: u16 = if entry.descriptor { 8 } else { 0 };
        let offset = out.len() as u32;
        let (local_crc, local_compressed, local_size) = if entry.descriptor { (0, 0, 0) } else { (crc, payload.len() as u32, entry.data.len() as u32) };
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0x21, 0]); // time, date
        out.extend_from_slice(&local_crc.to_le_bytes());
        out.extend_from_slice(&local_compressed.to_le_bytes());
        out.extend_from_slice(&local_size.to_le_bytes());
        out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(&payload);
        if entry.descriptor {
            out.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
        }
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&flags.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&[0, 0, 0x21, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        central.extend_from_slice(&(entry.data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // extra, comment, disk, internal attrs
        central.extend_from_slice(&((0o100644u32) << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(entry.name.as_bytes());
    }
    out.extend_from_slice(before_directory);
    let directory_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&directory_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

pub fn npm_json(json: &str) -> Vec<u8> {
    tar_gz(&[("package/package.json", json.as_bytes()), ("package/index.js", b"module.exports = 1;\n")])
}

pub fn npm(name: &str, version: &str) -> Vec<u8> {
    npm_json(&json!({ "name": name, "version": version, "dependencies": { "left-pad": "^1.3.0" } }).to_string())
}

pub fn crate_archive(name: &str, version: &str) -> Vec<u8> {
    let manifest = format!("[package]\nedition = \"2021\"\nname = \"{name}\"\nversion = \"{version}\"\n\n[dependencies.serde]\nversion = \"1\"\n");
    let root = format!("{name}-{version}");
    tar_gz(&[(&format!("{root}/Cargo.toml"), manifest.as_bytes()), (&format!("{root}/src/lib.rs"), b"pub fn f() {}\n")])
}

pub fn nuspec(id: &str, version: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<package xmlns=\"http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd\">\n  <metadata>\n    <id>{id}</id>\n    <version>{version}</version>\n    <authors>Tests</authors>\n    <license type=\"expression\">MIT</license>\n  </metadata>\n</package>\n"
    )
}

pub fn nupkg(id: &str, version: &str) -> Vec<u8> {
    let spec = nuspec(id, version);
    zip(&[ZipEntry::new(&format!("{id}.nuspec"), spec.as_bytes()), ZipEntry::new("lib/net8.0/Demo.dll", b"MZ")])
}

pub fn composer(name: &str, version: &str) -> Vec<u8> {
    let manifest = json!({ "name": name, "version": version, "require": { "php": ">=8.1" } }).to_string();
    zip(&[ZipEntry::new("composer.json", manifest.as_bytes()), ZipEntry::new("src/Demo.php", b"<?php\n")])
}

// ---------------------------------------------------------------------------------------------
// Running the binary

pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}\nstderr:\n{}", self.stdout(), self.stderr))
    }
    #[track_caller]
    pub fn assert_code(&self, code: i32) -> &Self {
        assert_eq!(self.code, code, "unexpected exit code\nstdout:\n{}\nstderr:\n{}", self.stdout(), self.stderr);
        self
    }
}

pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ppr-tool")
}

/// A command for the built binary with every ppr-tool, GitHub and colour variable removed.
pub fn tool(dir: &Path) -> Command {
    let mut command = Command::new(bin());
    command.current_dir(dir);
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy().into_owned();
        if ["PPR_", "GITHUB_", "ACTIONS_", "RUNNER_", "CLICOLOR"].iter().any(|p| key.starts_with(p)) || key == "NO_COLOR" {
            command.env_remove(&key);
        }
    }
    command
}

pub fn run(mut command: impl std::borrow::BorrowMut<Command>) -> Output {
    let output = command.borrow_mut().output().expect("run ppr-tool");
    Output { code: output.status.code().unwrap_or(-1), stdout: output.stdout, stderr: String::from_utf8_lossy(&output.stderr).into_owned() }
}

/// Runs `ppr-tool` with `args` in `dir`.
pub fn ppr(dir: &Path, args: &[&str]) -> Output {
    run(tool(dir).args(args))
}

pub fn scope_args() -> Vec<&'static str> {
    vec!["--product", "demo", "--variant", "sources", "--commit", COMMIT]
}

// ---------------------------------------------------------------------------------------------
// Mock registry

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn path(&self) -> &str {
        self.url.split('?').next().unwrap()
    }
}

/// Forced response: requests whose method matches and whose path contains `path` get `status`,
/// `times` times (usize::MAX for always).
#[derive(Debug, Clone)]
pub struct Failure {
    pub method: &'static str,
    pub path: &'static str,
    pub status: u16,
    pub times: usize,
}

#[derive(Default)]
pub struct MockState {
    pub requests: Vec<Recorded>,
    pub manifest: Option<Value>,
    pub digest: Option<String>,
    pub uploaded: BTreeSet<String>,
    pub committed: bool,
    pub failures: Vec<Failure>,
    pub session_expires_at: Option<String>,
    pub sessions_issued: usize,
}

pub struct MockRegistry {
    pub origin: String,
    pub state: Arc<Mutex<MockState>>,
    server: Arc<tiny_http::Server>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub const STATIC_TOKEN: &str = "static-publish-token";
pub const SESSION_TOKEN: &str = "session-token-from-oidc";
pub const GITHUB_REQUEST_TOKEN: &str = "github-request-token";
pub const GITHUB_OIDC_TOKEN: &str = "github-oidc-jwt";
pub const RELEASE_ID: &str = "rel_test1";

impl MockRegistry {
    pub fn start() -> MockRegistry {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let state = Arc::new(Mutex::new(MockState::default()));
        let thread = {
            let server = server.clone();
            let state = state.clone();
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    handle(request, &state);
                }
            })
        };
        MockRegistry { origin: format!("http://127.0.0.1:{port}"), state, server, thread: Some(thread) }
    }

    pub fn fail(&self, failure: Failure) {
        self.state.lock().unwrap().failures.push(failure);
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn count(&self, method: &str, path_contains: &str) -> usize {
        self.requests().iter().filter(|r| r.method == method && r.path().contains(path_contains)).count()
    }

    pub fn oidc_url(&self) -> String {
        format!("{}/github/token?api-version=2.0", self.origin)
    }
}

impl Drop for MockRegistry {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn respond(request: tiny_http::Request, status: u16, body: Value) {
    let header = tiny_http::Header::from_bytes("content-type", "application/json").unwrap();
    let mut response = tiny_http::Response::from_string(body.to_string()).with_status_code(status).with_header(header);
    if (300..400).contains(&status) {
        response.add_header(tiny_http::Header::from_bytes("location", "https://elsewhere.example.com/").unwrap());
    }
    let _ = request.respond(response);
}

fn handle(mut request: tiny_http::Request, state: &Arc<Mutex<MockState>>) {
    let mut body = Vec::new();
    let _ = request.as_reader().read_to_end(&mut body);
    let recorded = Recorded {
        method: request.method().as_str().to_string(),
        url: request.url().to_string(),
        headers: request.headers().iter().map(|h| (h.field.as_str().as_str().to_string(), h.value.as_str().to_string())).collect(),
        body,
    };
    let mut state = state.lock().unwrap();
    state.requests.push(recorded.clone());
    let path = recorded.path().to_string();
    if let Some(failure) = state.failures.iter_mut().find(|f| f.method == recorded.method && path.contains(f.path) && f.times > 0) {
        failure.times = failure.times.saturating_sub(1);
        let status = failure.status;
        drop(state);
        return respond(request, status, json!({ "error": "forced failure with secret-looking body" }));
    }
    let bearer = recorded.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").to_string();
    match (recorded.method.as_str(), path.as_str()) {
        ("GET", "/github/token") => {
            if bearer != GITHUB_REQUEST_TOKEN || !recorded.url.contains("audience=") {
                return respond(request, 401, json!({}));
            }
            respond(request, 200, json!({ "value": GITHUB_OIDC_TOKEN }))
        }
        ("POST", "/api/v1/auth/oidc") => {
            let body: Value = serde_json::from_slice(&recorded.body).unwrap_or(Value::Null);
            if body["token"] != GITHUB_OIDC_TOKEN {
                return respond(request, 401, json!({}));
            }
            state.sessions_issued += 1;
            let expires = state.session_expires_at.clone().unwrap_or_else(|| "2999-01-01T00:00:00Z".to_string());
            respond(request, 200, json!({ "token": SESSION_TOKEN, "expiresAt": expires }))
        }
        _ if path.starts_with("/api/v1/products/") => {
            if bearer != STATIC_TOKEN && bearer != SESSION_TOKEN {
                return respond(request, 401, json!({}));
            }
            let base = "/api/v1/products/demo/releases";
            let release = format!("{base}/{RELEASE_ID}");
            if recorded.method == "POST" && path == base {
                let digest = sha256_hex(&recorded.body);
                if recorded.header("idempotency-key") != Some(digest.as_str()) || recorded.header("content-type") != Some("application/json") {
                    return respond(request, 400, json!({}));
                }
                if state.digest.as_ref().is_some_and(|d| *d != digest) {
                    return respond(request, 409, json!({}));
                }
                state.manifest = Some(serde_json::from_slice(&recorded.body).unwrap());
                state.digest = Some(digest.clone());
                let uploaded: Vec<String> = state.uploaded.iter().cloned().collect();
                return respond(request, 200, json!({ "id": RELEASE_ID, "manifestSha256": digest, "uploaded": uploaded }));
            }
            if recorded.method == "PUT" && path.starts_with(&format!("{release}/artifacts/")) {
                let key = path.rsplit('/').next().unwrap().to_string();
                let digest = state.digest.clone().unwrap_or_default();
                let claimed = recorded.header("x-content-sha256").unwrap_or("");
                let length_ok = recorded.header("content-length").and_then(|v| v.parse::<usize>().ok()) == Some(recorded.body.len());
                if claimed != sha256_hex(&recorded.body) || !length_ok || recorded.header("idempotency-key") != Some(format!("{digest}:{key}").as_str()) {
                    return respond(request, 400, json!({}));
                }
                state.uploaded.insert(key);
                return respond(request, 200, json!({}));
            }
            if recorded.method == "GET" && path == release {
                let keys: Vec<String> = state
                    .manifest
                    .as_ref()
                    .map(|m| m["packages"].as_array().unwrap().iter().map(|p| p["key"].as_str().unwrap().to_string()).collect())
                    .unwrap_or_default();
                let complete = keys.iter().all(|k| state.uploaded.contains(k));
                return respond(request, 200, json!({ "manifestSha256": state.digest, "complete": complete }));
            }
            if recorded.method == "POST" && path == format!("{release}/commit") {
                let body: Value = serde_json::from_slice(&recorded.body).unwrap_or(Value::Null);
                let digest = state.digest.clone().unwrap_or_default();
                if body["manifestSha256"] != digest.as_str() || recorded.header("idempotency-key") != Some(format!("commit:{digest}").as_str()) {
                    return respond(request, 400, json!({}));
                }
                state.committed = true;
                return respond(request, 200, json!({ "published": true }));
            }
            respond(request, 404, json!({}))
        }
        _ => respond(request, 404, json!({})),
    }
}

/// Writes one archive per format into `dir` and returns their file names.
pub fn write_release(dir: &TempDir, version: &str) -> Vec<String> {
    let files = [
        (format!("npm/demo-{version}.tgz"), npm("@demo/sdk", version)),
        (format!("cargo/demo-{version}.crate"), crate_archive("demo", version)),
        (format!("nuget/Demo.Sdk.{version}.nupkg"), nupkg("Demo.Sdk", version)),
        (format!("composer/demo-{version}.zip"), composer("demo/sdk", version)),
    ];
    files
        .into_iter()
        .map(|(name, bytes)| {
            dir.write(&name, &bytes);
            name
        })
        .collect()
}
