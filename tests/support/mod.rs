//! Shared helpers for the integration tests: archive builders, a scratch directory, a runner for the
//! built binary and a mock registry that implements publication API v2 with a mock object store.
#![allow(dead_code)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
// Mock registry (publication API v2, docs/API.md) with a mock object store under /mock-s3/

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

/// A presigned upload URL issued by the mock.
#[derive(Debug, Clone)]
pub struct Ticket {
    pub key: String,
    /// Every PUT to an expired URL gets 403, like an S3 signature past its expiry.
    pub expired: bool,
}

/// Outcome of an asynchronous verification in the mock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ready,
    Failed,
    /// Ends without a verdict: the artifact goes back to `uploading` (for example a crashed worker).
    Abandoned,
}

#[derive(Default)]
pub struct MockState {
    pub origin: String,
    pub requests: Vec<Recorded>,
    pub manifest: Option<Value>,
    pub digest: Option<String>,
    /// Keys of verified (ready) artifacts.
    pub uploaded: BTreeSet<String>,
    /// Keys whose verification failed.
    pub failed: BTreeSet<String>,
    /// Keys in asynchronous verification: (status polls still answered "verifying", final outcome).
    pub verifying: BTreeMap<String, (usize, Verdict)>,
    pub tickets: BTreeMap<String, Ticket>,
    /// The newest uploadId per artifact key; older ones are stale.
    pub current: BTreeMap<String, String>,
    /// The mock bucket: stored bodies by uploadId.
    pub objects: BTreeMap<String, Vec<u8>>,
    pub issued: usize,
    pub committed: bool,
    pub failures: Vec<Failure>,
    pub session_expires_at: Option<String>,
    pub sessions_issued: usize,
    /// Storage PUTs that carried a credential or cookie. Must stay empty.
    pub leaks: Vec<String>,
    // Injections; each counter is consumed by one request.
    /// The next N upload URLs are born expired.
    pub expired_urls: usize,
    /// The next N completions answer 412 upload_missing (the stored object is discarded).
    pub missing_uploads: usize,
    /// The next N completions answer 202; the outcome (ready) shows after `async_polls` status polls.
    pub async_ready: usize,
    /// The next N completions answer 202; the outcome (failed) shows after `async_polls` status polls.
    pub async_failed: usize,
    pub async_polls: usize,
    /// The next N completions answer 409 stale_upload although another process finished the artifact.
    pub completed_elsewhere: usize,
    /// The next N completions answer 409 stale_upload.
    pub stale_uploads: usize,
    /// The next N upload-url requests find the artifact being verified elsewhere (409 verifying, then ready).
    pub verifying_elsewhere: usize,
    /// Like `verifying_elsewhere`, but that verification ends without a verdict.
    pub abandoned_elsewhere: usize,
    /// The next N storage PUTs lose the connection: the mock reads the body and closes without answering.
    pub dropped_puts: usize,
    /// Replaces the URL in upload tickets.
    pub ticket_url: Option<String>,
    /// Extra headers in upload tickets.
    pub ticket_headers: Vec<(String, String)>,
}

impl MockState {
    fn package(&self, key: &str) -> Option<Value> {
        self.manifest.as_ref()?["packages"].as_array()?.iter().find(|p| p["key"] == key).cloned()
    }
    fn keys(&self) -> Vec<String> {
        self.manifest.as_ref().map(|m| m["packages"].as_array().unwrap().iter().map(|p| p["key"].as_str().unwrap().to_string()).collect()).unwrap_or_default()
    }
    fn state_of(&self, key: &str) -> &'static str {
        if self.uploaded.contains(key) {
            "ready"
        } else if self.verifying.contains_key(key) {
            "verifying"
        } else if self.failed.contains(key) {
            "failed"
        } else if self.current.contains_key(key) {
            "uploading"
        } else {
            "declared"
        }
    }
}

fn take(counter: &mut usize) -> bool {
    let hit = *counter > 0;
    *counter = counter.saturating_sub(1);
    hit
}

pub struct MockRegistry {
    pub origin: String,
    pub state: Arc<Mutex<MockState>>,
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub const STATIC_TOKEN: &str = "static-publish-token";
pub const SESSION_TOKEN: &str = "session-token-from-oidc";
pub const GITHUB_REQUEST_TOKEN: &str = "github-request-token";
pub const GITHUB_OIDC_TOKEN: &str = "github-oidc-jwt";
pub const RELEASE_ID: &str = "01k5z8e3v4x9b2c6d7f8g9h0jk";
/// Marker in every presigned URL; it must never show up in the tool's output.
pub const SIGNATURE: &str = "X-Amz-Signature=mock-signature";

impl MockRegistry {
    /// A thread-per-connection HTTP/1.1 server. (tiny_http's worker pool can starve a new connection
    /// while its threads sit on idle keep-alive connections, which hung parallel uploads.)
    pub fn start() -> MockRegistry {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("http://127.0.0.1:{}", address.port());
        let state = Arc::new(Mutex::new(MockState { origin: origin.clone(), async_polls: 1, ..MockState::default() }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let state = state.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let state = state.clone();
                    std::thread::spawn(move || serve(stream, &state));
                }
            })
        };
        MockRegistry { origin, state, address, stop, thread: Some(thread) }
    }

    pub fn fail(&self, failure: Failure) {
        self.state.lock().unwrap().failures.push(failure);
    }

    /// Changes the mock's state (injections) under its lock.
    pub fn with<R>(&self, change: impl FnOnce(&mut MockState) -> R) -> R {
        change(&mut self.state.lock().unwrap())
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
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub close: bool,
    /// Close the connection without any response.
    pub abort: bool,
}

fn serve(stream: TcpStream, state: &Arc<Mutex<MockState>>) {
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    loop {
        let reply = match read_request(&mut reader) {
            None => return,
            Some(Ok(recorded)) => handle(recorded, state),
            Some(Err(reply)) => reply,
        };
        if reply.abort {
            return;
        }
        let mut head = format!("HTTP/1.1 {} Mock\r\n", reply.status);
        for (name, value) in &reply.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!("content-length: {}\r\nconnection: {}\r\n\r\n", reply.body.len(), if reply.close { "close" } else { "keep-alive" }));
        if writer.write_all(head.as_bytes()).and_then(|_| writer.write_all(&reply.body)).and_then(|_| writer.flush()).is_err() || reply.close {
            return;
        }
    }
}

/// Parses one request; `None` when the client closed the connection.
fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Result<Recorded, Reply>> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.trim_end().splitn(3, ' ');
    let (method, url) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let find = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    // Object storage demands a Content-Length on presigned PUTs; the registry gets sized bodies too.
    if find("transfer-encoding").is_some() {
        return Some(Err(Reply { status: 411, headers: Vec::new(), body: Vec::new(), close: true, abort: false }));
    }
    let length = find("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Ok(Recorded { method, url, headers, body }))
}

fn respond(status: u16, body: Value) -> Reply {
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    // A cookie the tool must never send back, to registry or storage.
    headers.push(("set-cookie".to_string(), "ppr_mock=1; Path=/".to_string()));
    if (300..400).contains(&status) {
        headers.push(("location".to_string(), "https://elsewhere.example.com/".to_string()));
    }
    Reply { status, headers, body: body.to_string().into_bytes(), close: false, abort: false }
}

fn error(status: u16, code: &str) -> Reply {
    respond(status, json!({ "error": { "message": format!("mock {code}"), "code": code } }))
}

/// S3-style XML error from the mock object store.
fn storage_error(status: u16, code: &str) -> Reply {
    let body = format!("<Error><Code>{code}</Code><Message>secret-looking storage body</Message></Error>");
    Reply { status, headers: vec![("content-type".to_string(), "application/xml".to_string())], body: body.into_bytes(), close: false, abort: false }
}

fn handle(recorded: Recorded, state: &Arc<Mutex<MockState>>) -> Reply {
    let mut state = state.lock().unwrap();
    state.requests.push(recorded.clone());
    let path = recorded.path().to_string();
    if let Some(failure) = state.failures.iter_mut().find(|f| f.method == recorded.method && path.contains(f.path) && f.times > 0) {
        failure.times = failure.times.saturating_sub(1);
        return respond(failure.status, json!({ "error": "forced failure with secret-looking body" }));
    }
    if let Some(upload_id) = path.strip_prefix("/mock-s3/") {
        return storage(&recorded, &mut state, upload_id);
    }
    let bearer = recorded.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").to_string();
    match (recorded.method.as_str(), path.as_str()) {
        ("GET", "/github/token") => {
            if bearer != GITHUB_REQUEST_TOKEN || !recorded.url.contains("audience=") {
                return respond(401, json!({}));
            }
            respond(200, json!({ "value": GITHUB_OIDC_TOKEN }))
        }
        ("POST", "/api/v2/auth/oidc") => {
            let body: Value = serde_json::from_slice(&recorded.body).unwrap_or(Value::Null);
            if body["token"] != GITHUB_OIDC_TOKEN {
                return respond(401, json!({}));
            }
            state.sessions_issued += 1;
            let expires = state.session_expires_at.clone().unwrap_or_else(|| "2999-01-01T00:00:00.000Z".to_string());
            respond(200, json!({ "token": SESSION_TOKEN, "expiresAt": expires }))
        }
        _ if path.starts_with("/api/v2/products/") => {
            if bearer != STATIC_TOKEN && bearer != SESSION_TOKEN {
                return error(401, "unauthenticated");
            }
            publication(&recorded, &mut state, &path)
        }
        _ => respond(404, json!({})),
    }
}

fn publication(recorded: &Recorded, state: &mut MockState, path: &str) -> Reply {
    let base = "/api/v2/products/demo/releases";
    let release = format!("{base}/{RELEASE_ID}");
    let digest = state.digest.clone().unwrap_or_default();
    if recorded.method == "POST" && path == base {
        let digest = sha256_hex(&recorded.body);
        if recorded.header("idempotency-key") != Some(digest.as_str()) || recorded.header("content-type") != Some("application/json") {
            return error(400, "bad_request");
        }
        if state.digest.as_ref().is_some_and(|d| *d != digest) {
            return error(409, "conflict");
        }
        state.manifest = Some(serde_json::from_slice(&recorded.body).unwrap());
        state.digest = Some(digest.clone());
        let (uploaded, pending): (Vec<String>, Vec<String>) = state.keys().into_iter().partition(|k| state.uploaded.contains(k));
        return respond(200, json!({ "id": RELEASE_ID, "manifestSha256": digest, "uploaded": uploaded, "pending": pending }));
    }
    if recorded.method == "GET" && path == release {
        // Each poll brings asynchronous verifications one step closer to their outcome.
        let settled: Vec<(String, Verdict)> = state.verifying.iter().filter(|(_, (polls, _))| *polls == 0).map(|(k, (_, v))| (k.clone(), *v)).collect();
        for (key, verdict) in settled {
            state.verifying.remove(&key);
            match verdict {
                Verdict::Ready => state.uploaded.insert(key),
                Verdict::Failed => state.failed.insert(key),
                Verdict::Abandoned => false,
            };
        }
        let keys = state.keys();
        let artifacts: Vec<Value> = keys
            .iter()
            .map(|k| match state.state_of(k) {
                "failed" => json!({ "key": k, "state": "failed", "error": "Archive metadata differs from manifest" }),
                other => json!({ "key": k, "state": other }),
            })
            .collect();
        for entry in state.verifying.values_mut() {
            entry.0 = entry.0.saturating_sub(1);
        }
        let complete = !keys.is_empty() && keys.iter().all(|k| state.uploaded.contains(k));
        return respond(200, json!({ "manifestSha256": state.digest, "complete": complete, "artifacts": artifacts }));
    }
    if recorded.method == "POST" && path == format!("{release}/commit") {
        let body: Value = serde_json::from_slice(&recorded.body).unwrap_or(Value::Null);
        if body["manifestSha256"] != digest.as_str() || recorded.header("idempotency-key") != Some(format!("commit:{digest}").as_str()) {
            return error(409, "manifest_mismatch");
        }
        if !state.keys().iter().all(|k| state.uploaded.contains(k)) {
            return error(409, "incomplete");
        }
        state.committed = true;
        return respond(200, json!({}));
    }
    let Some(rest) = path.strip_prefix(&format!("{release}/artifacts/")) else {
        return error(404, "not_found");
    };
    let (key, action) = rest.split_once('/').unwrap_or((rest, ""));
    if recorded.method != "POST" || state.package(key).is_none() {
        return error(404, "not_found");
    }
    if recorded.header("idempotency-key") != Some(format!("{digest}:{key}").as_str()) || recorded.header("content-type") != Some("application/json") {
        return error(400, "bad_request");
    }
    match action {
        "upload-url" => issue(state, key),
        "complete" => complete(recorded, state, key),
        _ => error(404, "not_found"),
    }
}

fn issue(state: &mut MockState, key: &str) -> Reply {
    if state.uploaded.contains(key) {
        return respond(200, json!({ "ready": true }));
    }
    for (counter, verdict) in [(&mut state.verifying_elsewhere, Verdict::Ready), (&mut state.abandoned_elsewhere, Verdict::Abandoned)] {
        if take(counter) {
            let polls = state.async_polls;
            state.verifying.insert(key.to_string(), (polls, verdict));
            break;
        }
    }
    if state.verifying.contains_key(key) {
        return error(409, "verifying");
    }
    state.issued += 1;
    let upload_id = format!("01upload{:018}", state.issued);
    let expired = take(&mut state.expired_urls);
    state.tickets.insert(upload_id.clone(), Ticket { key: key.to_string(), expired });
    state.current.insert(key.to_string(), upload_id.clone());
    state.failed.remove(key);
    let url = state.ticket_url.clone().unwrap_or_else(|| format!("{}/mock-s3/{upload_id}?X-Amz-Expires=1800&{SIGNATURE}", state.origin));
    let mut headers = serde_json::Map::new();
    headers.insert("Content-Type".into(), json!("application/octet-stream"));
    headers.insert("x-mock-signed".into(), json!(upload_id));
    for (name, value) in &state.ticket_headers {
        headers.insert(name.clone(), json!(value));
    }
    respond(200, json!({ "ready": false, "uploadId": upload_id, "url": url, "method": "PUT", "headers": headers, "expiresAt": "2999-01-01T00:30:00.000Z" }))
}

fn complete(recorded: &Recorded, state: &mut MockState, key: &str) -> Reply {
    let body: Value = serde_json::from_slice(&recorded.body).unwrap_or(Value::Null);
    let Some(upload_id) = body["uploadId"].as_str().map(str::to_string) else {
        return error(422, "invalid");
    };
    let current = state.current.get(key) == Some(&upload_id);
    if state.uploaded.contains(key) && current {
        return respond(200, json!({ "ready": true }));
    }
    if take(&mut state.completed_elsewhere) {
        state.uploaded.insert(key.to_string());
        return error(409, "stale_upload");
    }
    if take(&mut state.stale_uploads) || !current {
        return error(409, "stale_upload");
    }
    if take(&mut state.missing_uploads) {
        state.objects.remove(&upload_id);
        return error(412, "upload_missing");
    }
    let package = state.package(key).unwrap();
    let Some(object) = state.objects.get(&upload_id) else {
        return error(412, "upload_missing");
    };
    if package["size"].as_u64() != Some(object.len() as u64) {
        return error(412, "upload_missing");
    }
    if package["sha256"].as_str() != Some(sha256_hex(object).as_str()) {
        state.failed.insert(key.to_string());
        return error(422, "rejected");
    }
    for (counter, verdict) in [(&mut state.async_ready, Verdict::Ready), (&mut state.async_failed, Verdict::Failed)] {
        if take(counter) {
            let polls = state.async_polls;
            state.verifying.insert(key.to_string(), (polls, verdict));
            return respond(202, json!({ "state": "verifying" }));
        }
    }
    state.uploaded.insert(key.to_string());
    respond(200, json!({ "ready": true }))
}

/// The mock object store: presigned PUTs only, never with the registry credential.
fn storage(recorded: &Recorded, state: &mut MockState, upload_id: &str) -> Reply {
    if recorded.header("authorization").is_some() || recorded.header("cookie").is_some() || recorded.header("proxy-authorization").is_some() {
        state.leaks.push(recorded.url.clone());
        return storage_error(400, "CredentialLeak");
    }
    if recorded.method != "PUT" {
        return storage_error(405, "MethodNotAllowed");
    }
    if take(&mut state.dropped_puts) {
        return Reply { status: 0, headers: Vec::new(), body: Vec::new(), close: true, abort: true };
    }
    let Some(ticket) = state.tickets.get(upload_id).cloned() else {
        return storage_error(404, "NoSuchUpload");
    };
    if ticket.expired {
        return storage_error(403, "AccessDenied");
    }
    // The signed headers must arrive exactly as issued.
    if recorded.header("x-mock-signed") != Some(upload_id)
        || recorded.header("content-type") != Some("application/octet-stream")
        || !recorded.url.contains(SIGNATURE)
    {
        return storage_error(403, "SignatureDoesNotMatch");
    }
    if recorded.header("content-length").and_then(|v| v.parse::<usize>().ok()) != Some(recorded.body.len()) {
        return storage_error(400, "IncompleteBody");
    }
    state.objects.insert(upload_id.to_string(), recorded.body.clone());
    Reply { status: 200, headers: vec![("etag".to_string(), format!("\"{}\"", sha256_hex(&recorded.body)))], body: Vec::new(), close: false, abort: false }
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
