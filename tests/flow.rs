//! End-to-end publication against a mock registry (publication API v2) and its mock object store:
//! stage, presigned uploads, verify, commit, status and publish.

mod support;

use serde_json::Value;
use std::process::Command;
use support::*;

struct Fixture {
    dir: TempDir,
    registry: MockRegistry,
    files: Vec<String>,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = TempDir::new();
        let files = write_release(&dir, "1.2.3");
        Fixture { dir, registry: MockRegistry::start(), files }
    }

    /// A command with the static token and registry configured; verification polls are fast.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut command = tool(self.dir.path());
        command.args(args).env("PPR_TOKEN", STATIC_TOKEN).env("PPR_REGISTRY", &self.registry.origin).env("PPR_TOOL_TEST_POLL_MS", "10");
        command
    }

    fn stage_args(&self) -> Vec<String> {
        let mut args: Vec<String> = ["stage"].iter().chain(scope_args().iter()).map(|s| s.to_string()).collect();
        args.extend(self.files.iter().cloned());
        args
    }

    fn stage(&self) -> Output {
        self.stage_with(&[])
    }

    fn stage_with(&self, extra: &[&str]) -> Output {
        let mut args = self.stage_args();
        args.extend(extra.iter().map(|s| s.to_string()));
        run(self.cmd(&args.iter().map(String::as_str).collect::<Vec<_>>()))
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.dir.path().join(".ppr-tool/state.json")).unwrap()).unwrap()
    }
}

/// A verification command that succeeds everywhere: the tool itself.
fn passing_check() -> Vec<&'static str> {
    vec!["--", bin(), "--version"]
}

/// A verification command that exits 2 everywhere: the tool without arguments.
fn failing_check() -> Vec<&'static str> {
    vec!["--", bin(), "validate"]
}

#[test]
fn stages_verifies_and_commits() {
    let f = Fixture::new();
    let staged = f.stage();
    staged.assert_code(0);
    assert!(staged.stdout().contains(&format!("Release ID: {RELEASE_ID}")));
    assert!(!staged.stdout().contains(STATIC_TOKEN) && !staged.stderr.contains(STATIC_TOKEN));

    let state = f.state();
    assert_eq!(state["schemaVersion"], 2);
    assert_eq!(state["releaseId"], RELEASE_ID);
    assert_eq!(state["preview"], format!("{}/preview/{RELEASE_ID}", f.registry.origin));
    assert_eq!(state["archives"].as_array().unwrap().len(), 4);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(f.dir.path().join(".ppr-tool/state.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let requests = f.registry.requests();
    let stage = requests.iter().find(|r| r.method == "POST" && r.path().ends_with("/releases")).unwrap();
    let wire = run(tool(f.dir.path()).args(["validate", "--json"]).args(scope_args()).args(&f.files));
    assert_eq!(stage.body, wire.stdout, "stage posts exactly the validate --json bytes");
    assert_eq!(f.registry.count("POST", "/upload-url"), 4);
    assert_eq!(f.registry.count("PUT", "/mock-s3/"), 4);
    assert_eq!(f.registry.count("POST", "/complete"), 4);
    assert_eq!(f.registry.count("PUT", "/api/"), 0, "archives never pass through the registry");
    assert_eq!(f.registry.with(|s| s.uploaded.len()), 4);

    let early = run(f.cmd(&["commit"]));
    early.assert_code(7);
    assert!(early.stderr.contains("verify"));

    run(f.cmd(&[&["verify"], passing_check().as_slice()].concat())).assert_code(0);
    assert_eq!(f.state()["testedDigest"], f.state()["digest"]);
    let preview: Value = serde_json::from_slice(&std::fs::read(f.dir.path().join(".ppr-tool/preview.json")).unwrap()).unwrap();
    assert_eq!(preview["releaseId"], RELEASE_ID);
    assert_eq!(preview["cargoIndex"], format!("{}/cargo/index/", f.registry.origin));
    assert_eq!(preview["packages"].as_array().unwrap().len(), 4);

    let status = run(f.cmd(&["status", "--json"]));
    status.assert_code(0);
    let report = status.json();
    assert_eq!(report["complete"], true);
    let artifacts = report["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 4);
    assert!(artifacts.iter().all(|a| a["state"] == "ready"));
    assert_eq!(report["digestMatches"], true);
    assert_eq!(report["tested"], true);
    assert!(!status.stdout().contains(STATIC_TOKEN));

    let committed = run(f.cmd(&["commit", "--json", "--release", RELEASE_ID]));
    committed.assert_code(0);
    assert_eq!(committed.json()["published"], true);
    assert!(f.registry.state.lock().unwrap().committed);
}

#[test]
fn resumes_without_reuploading() {
    let f = Fixture::new();
    f.stage().assert_code(0);
    assert_eq!(f.registry.count("PUT", "/mock-s3/"), 4);
    // The registry reports every package as uploaded; a second stage only re-checks the status.
    let again = f.stage();
    again.assert_code(0);
    assert_eq!(f.registry.count("POST", "/upload-url"), 4);
    assert_eq!(f.registry.count("PUT", "/mock-s3/"), 4);
    assert!(again.stderr.contains("0 uploaded, 4 already present"));

    // A partially uploaded draft resumes with the pending packages only.
    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/complete", status: 422, times: 1 });
    f.stage_with(&["-j", "1"]).assert_code(5);
    let done = f.registry.with(|s| s.uploaded.len());
    let resumed = f.stage();
    resumed.assert_code(0);
    assert!(resumed.stderr.contains(&format!("{} uploaded, {done} already present", 4 - done)), "{}", resumed.stderr);
}

#[test]
fn verify_failures_and_changed_artifacts() {
    let f = Fixture::new();
    f.stage().assert_code(0);
    let failed = run(f.cmd(&[&["verify"], failing_check().as_slice()].concat()));
    failed.assert_code(6);
    assert!(failed.stderr.contains("exit 2"));
    assert!(f.state().get("testedDigest").is_none());

    run(f.cmd(&["verify", "--release", "rel_other", "--", bin(), "--version"])).assert_code(7);
    run(f.cmd(&["verify", "--registry", "https://other.example.com", "--", bin(), "--version"])).assert_code(7);

    // Rebuilding with identical bytes is fine; different contents are not.
    f.dir.write(&f.files[1], &crate_archive("demo", "1.2.3"));
    run(f.cmd(&[&["verify"], passing_check().as_slice()].concat())).assert_code(0);
    f.dir.write(&f.files[0], &npm_json(r#"{"name":"@demo/sdk","version":"1.2.3","description":"changed"}"#));
    let drifted = run(f.cmd(&[&["verify"], passing_check().as_slice()].concat()));
    drifted.assert_code(7);
    assert!(drifted.stderr.contains("artifacts changed after staging"));
}

#[test]
fn registry_errors_map_to_exit_codes() {
    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/releases", status: 401, times: 1 });
    let unauthorized = f.stage();
    unauthorized.assert_code(4);
    assert!(!unauthorized.stderr.contains("secret-looking"), "response bodies are never printed");

    f.registry.fail(Failure { method: "POST", path: "/releases", status: 503, times: 2 });
    f.stage().assert_code(0);

    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/releases", status: 500, times: usize::MAX });
    let broken = f.stage();
    broken.assert_code(5);
    assert_eq!(f.registry.count("POST", "/releases"), 4, "four attempts");

    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/releases", status: 302, times: 1 });
    assert!(f.stage().assert_code(5).stderr.contains("redirect"));

    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/complete", status: 422, times: usize::MAX });
    let rejected = f.stage();
    rejected.assert_code(5);
    assert!(rejected.stderr.contains("audit log"), "{}", rejected.stderr);

    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/upload-url", status: 409, times: usize::MAX });
    assert!(f.stage().assert_code(5).stderr.contains("no longer a draft"));

    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/upload-url", status: 503, times: 2 });
    f.stage().assert_code(0);

    let f = Fixture::new();
    let args = f.stage_args();
    let no_credentials = run(tool(f.dir.path()).args(&args).env("PPR_REGISTRY", &f.registry.origin));
    no_credentials.assert_code(4);
    let no_registry = run(tool(f.dir.path()).args(&args).env("PPR_TOKEN", STATIC_TOKEN));
    no_registry.assert_code(2);
    run(f.cmd(&["status"])).assert_code(7);
}

fn oidc(f: &Fixture, command: &mut Command) {
    command
        .env_remove("PPR_TOKEN")
        .env("ACTIONS_ID_TOKEN_REQUEST_URL", f.registry.oidc_url())
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", GITHUB_REQUEST_TOKEN)
        .env("PPR_TOOL_TEST_ALLOW_HTTP_OIDC", "1");
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "needs the debug-only PPR_TOOL_TEST_ALLOW_HTTP_OIDC hook")]
fn github_actions_oidc_and_outputs() {
    let f = Fixture::new();
    let outputs = f.dir.write("gh/output", b"");
    let env_file = f.dir.write("gh/env", b"");
    let summary = f.dir.write("gh/summary", b"");
    let args = f.stage_args();
    let mut stage = f.cmd(&[&args.iter().map(String::as_str).collect::<Vec<_>>()[..], &["--json"]].concat());
    oidc(&f, &mut stage);
    stage.env("GITHUB_ACTIONS", "true").env("GITHUB_OUTPUT", &outputs).env("GITHUB_ENV", &env_file).env("GITHUB_STEP_SUMMARY", &summary);
    let staged = run(&mut stage);
    staged.assert_code(0);
    assert_eq!(staged.json()["releaseId"], RELEASE_ID, "stdout holds only JSON");
    assert!(staged.stderr.contains("::add-mask::"));
    assert!(staged.stderr.contains("::group::Uploading 4 packages"));
    assert!(staged.stderr.lines().all(|l| !l.contains(SESSION_TOKEN) || l.starts_with("::add-mask::")), "the session token only appears in mask commands");

    let requests = f.registry.requests();
    let token_request = requests.iter().find(|r| r.path() == "/github/token").unwrap();
    assert!(token_request.url.contains("api-version=2.0"));
    assert!(token_request.url.contains("audience=http%3A%2F%2F127.0.0.1%3A"), "{}", token_request.url);
    let exchange: Value = serde_json::from_slice(&requests.iter().find(|r| r.path() == "/api/v2/auth/oidc").unwrap().body).unwrap();
    assert_eq!(exchange["product"], "demo");
    assert_eq!(exchange["version"], "1.2.3");
    assert_eq!(f.state()["token"], SESSION_TOKEN);

    let written = std::fs::read_to_string(&outputs).unwrap();
    assert!(written.contains(&format!("release-id={RELEASE_ID}")) && written.contains("preview-url="));
    assert!(std::fs::read_to_string(&env_file).unwrap().contains(&format!("PPR_RELEASE_ID={RELEASE_ID}")));
    let markdown = std::fs::read_to_string(&summary).unwrap();
    assert!(markdown.contains("| Format | Package |") && markdown.contains("`Demo.Sdk`"));

    let status = run(f.cmd(&["status", "--json"]).env_remove("PPR_TOKEN"));
    status.assert_code(0);
    assert!(!status.stdout().contains(SESSION_TOKEN));
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "needs the debug-only PPR_TOOL_TEST_ALLOW_HTTP_OIDC hook")]
fn oidc_failures_are_auth_errors() {
    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/api/v2/auth/oidc", status: 400, times: usize::MAX });
    let args = f.stage_args();
    let mut stage = f.cmd(&args.iter().map(String::as_str).collect::<Vec<_>>());
    oidc(&f, &mut stage);
    run(&mut stage).assert_code(4);

    // No matching publisher trust: point at the trust configuration, not at the credential.
    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/api/v2/auth/oidc", status: 403, times: usize::MAX });
    let mut stage = f.cmd(&args.iter().map(String::as_str).collect::<Vec<_>>());
    oidc(&f, &mut stage);
    let denied = run(&mut stage);
    denied.assert_code(4);
    assert!(denied.stderr.contains("no publisher trust") && denied.stderr.contains("renamed or transferred"), "{}", denied.stderr);
    assert!(!denied.stderr.contains("credential is invalid"), "{}", denied.stderr);

    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/api/v2/auth/oidc", status: 401, times: usize::MAX });
    let mut stage = f.cmd(&args.iter().map(String::as_str).collect::<Vec<_>>());
    oidc(&f, &mut stage);
    assert!(run(&mut stage).assert_code(4).stderr.contains("token audience"));

    let f = Fixture::new();
    f.registry.fail(Failure { method: "GET", path: "/github/token", status: 302, times: usize::MAX });
    let mut stage = f.cmd(&args.iter().map(String::as_str).collect::<Vec<_>>());
    oidc(&f, &mut stage);
    run(&mut stage).assert_code(4);

    let f = Fixture::new();
    let mut foreign = f.cmd(&args.iter().map(String::as_str).collect::<Vec<_>>());
    oidc(&f, &mut foreign);
    foreign.env_remove("PPR_TOOL_TEST_ALLOW_HTTP_OIDC");
    assert!(run(&mut foreign).assert_code(4).stderr.contains("unexpected GitHub OIDC endpoint"));
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "needs the debug-only PPR_TOOL_TEST_ALLOW_HTTP_OIDC hook")]
fn refreshes_expiring_sessions() {
    let f = Fixture::new();
    f.registry.state.lock().unwrap().session_expires_at = Some(ppr_tool::time::format_rfc3339(ppr_tool::time::now() + 5));
    let args = f.stage_args();
    let mut stage = f.cmd(&args.iter().map(String::as_str).collect::<Vec<_>>());
    oidc(&f, &mut stage);
    run(&mut stage).assert_code(0);
    assert_eq!(f.registry.state.lock().unwrap().sessions_issued, 1);
    let mut status = f.cmd(&["status"]);
    oidc(&f, &mut status);
    run(&mut status).assert_code(0);
    assert_eq!(f.registry.state.lock().unwrap().sessions_issued, 2, "a session within 30 s of expiry is replaced");
}

#[test]
fn publish_runs_the_whole_flow() {
    let f = Fixture::new();
    let mut args: Vec<&str> = vec!["publish", "--json"];
    args.extend(scope_args());
    args.extend(f.files.iter().map(String::as_str));
    args.extend(passing_check());
    let published = run(f.cmd(&args));
    published.assert_code(0);
    let report = published.json();
    assert_eq!(report["published"], true);
    assert_eq!(report["uploaded"], 4);
    assert!(f.registry.state.lock().unwrap().committed);

    let f = Fixture::new();
    let mut args: Vec<&str> = vec!["publish", "--json", "-j", "1"];
    args.extend(scope_args());
    args.extend(f.files.iter().map(String::as_str));
    args.extend(failing_check());
    let failed = run(f.cmd(&args));
    failed.assert_code(6);
    let error = &failed.json()["error"];
    assert_eq!(error["kind"], "verify-failed");
    assert_eq!(error["childExitCode"], 2);
    assert!(!f.registry.state.lock().unwrap().committed);
}

#[cfg(unix)]
#[test]
fn verification_command_environment() {
    let f = Fixture::new();
    f.stage().assert_code(0);
    let script = format!(
        "test -f \"$PPR_PREVIEW\" && case \"$PPR_PREVIEW\" in /*) ;; *) exit 9;; esac && test \"$PPR_DOWNLOAD_TOKEN\" = {STATIC_TOKEN} && test \"$PPR_RELEASE_ID\" = {RELEASE_ID} && test \"$PPR_PREVIEW_URL\" = {}/preview/{RELEASE_ID} && cd / && test -f \"$PPR_PREVIEW\"",
        f.registry.origin
    );
    run(f.cmd(&["verify", "--", "sh", "-c", &script])).assert_code(0);
}

#[test]
fn archives_go_straight_to_storage_without_credentials() {
    let f = Fixture::new();
    let staged = f.stage();
    staged.assert_code(0);
    let requests = f.registry.requests();
    let puts: Vec<&Recorded> = requests.iter().filter(|r| r.path().starts_with("/mock-s3/")).collect();
    assert_eq!(puts.len(), 4);
    for put in &puts {
        assert_eq!(put.method, "PUT");
        assert!(put.header("authorization").is_none() && put.header("cookie").is_none(), "{:?}", put.headers);
        assert!(put.url.contains(SIGNATURE), "the presigned query string is kept");
        assert_eq!(put.header("content-type"), Some("application/octet-stream"));
        assert_eq!(put.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("content-type")).count(), 1, "the signed Content-Type is sent once");
        assert_eq!(put.header("x-mock-signed"), Some(put.path().trim_start_matches("/mock-s3/")), "signed headers are sent as issued");
        assert_eq!(put.header("content-length"), Some(put.body.len().to_string().as_str()));
        assert!(put.header("transfer-encoding").is_none() && put.header("idempotency-key").is_none());
    }
    assert!(f.registry.with(|s| s.leaks.is_empty()));
    assert!(requests.iter().all(|r| r.header("cookie").is_none()), "cookies are never stored or replayed");

    // The mock bucket holds exactly the archive bytes.
    let mut stored: Vec<String> = f.registry.with(|s| s.objects.values().map(|b| sha256_hex(b)).collect());
    let mut archives: Vec<String> = f.files.iter().map(|name| sha256_hex(&std::fs::read(f.dir.path().join(name)).unwrap())).collect();
    stored.sort();
    archives.sort();
    assert_eq!(stored, archives);

    // Registry calls carry the credential, the v2 idempotency keys and the uploadId of the PUT.
    let digest = f.state()["digest"].as_str().unwrap().to_string();
    for call in requests.iter().filter(|r| r.path().ends_with("/upload-url") || r.path().ends_with("/complete")) {
        assert_eq!(call.header("authorization"), Some(format!("Bearer {STATIC_TOKEN}").as_str()));
        let key = call.path().rsplit('/').nth(1).unwrap();
        assert_eq!(call.header("idempotency-key"), Some(format!("{digest}:{key}").as_str()));
    }
    for complete in requests.iter().filter(|r| r.path().ends_with("/complete")) {
        let body: Value = serde_json::from_slice(&complete.body).unwrap();
        let upload_id = body["uploadId"].as_str().unwrap();
        assert!(puts.iter().any(|p| p.path() == format!("/mock-s3/{upload_id}")));
    }
    assert!(!staged.stderr.contains("mock-signature") && !staged.stdout().contains("mock-signature"), "presigned URLs are never printed");
}

#[test]
fn missing_uploads_get_one_new_url() {
    let f = Fixture::new();
    f.registry.with(|s| s.missing_uploads = 1);
    f.stage().assert_code(0);
    assert_eq!(f.registry.count("POST", "/upload-url"), 5);
    assert_eq!(f.registry.count("PUT", "/mock-s3/"), 5);
    assert_eq!(f.registry.with(|s| s.uploaded.len()), 4);

    let f = Fixture::new();
    f.registry.with(|s| s.missing_uploads = 2);
    let failed = f.stage_with(&["--json", "-j", "1"]);
    failed.assert_code(5);
    let error = &failed.json()["error"];
    assert_eq!(error["httpStatus"], 412);
    assert_eq!(error["code"], "upload_missing");
    assert_eq!(f.registry.count("POST", "/upload-url"), 2, "only one fresh URL");
}

#[test]
fn expired_upload_urls_are_renewed_once() {
    let f = Fixture::new();
    f.registry.with(|s| s.expired_urls = 1);
    f.stage().assert_code(0);
    assert_eq!(f.registry.count("POST", "/upload-url"), 5);
    assert_eq!(f.registry.count("PUT", "/mock-s3/"), 5);
    assert_eq!(f.registry.with(|s| s.uploaded.len()), 4);

    let f = Fixture::new();
    f.registry.with(|s| s.expired_urls = 2);
    let failed = f.stage_with(&["--json", "-j", "1"]);
    failed.assert_code(5);
    let error = &failed.json()["error"];
    assert_eq!(error["kind"], "registry", "a storage 403 is not a credential problem");
    assert_eq!(error["storageStatus"], 403);
    assert!(!failed.stderr.contains("secret-looking"), "storage bodies are never printed");
    assert_eq!(f.registry.count("POST", "/upload-url"), 2);
}

#[test]
fn dropped_storage_connections_get_one_new_url() {
    // Storage may close the connection instead of answering 403 when it refuses a large body early.
    let f = Fixture::new();
    f.registry.with(|s| s.dropped_puts = 4);
    f.stage_with(&["-j", "1"]).assert_code(0);
    let puts: Vec<String> = f.registry.requests().iter().filter(|r| r.path().starts_with("/mock-s3/")).map(|r| r.url.clone()).collect();
    assert_eq!(puts.len(), 8);
    assert!(puts[..4].iter().all(|u| *u == puts[0]), "the same URL is retried first");
    assert_ne!(puts[4], puts[0], "then one fresh URL");
    assert_eq!(f.registry.count("POST", "/upload-url"), 5);
    assert_eq!(f.registry.with(|s| s.uploaded.len()), 4);

    let f = Fixture::new();
    f.registry.with(|s| s.dropped_puts = usize::MAX);
    let failed = f.stage_with(&["-j", "1"]);
    failed.assert_code(5);
    assert!(failed.stderr.contains("upload to object storage failed"), "{}", failed.stderr);
    assert_eq!(f.registry.count("POST", "/upload-url"), 2, "only one fresh URL");
    assert_eq!(f.registry.count("PUT", "/mock-s3/"), 8);
    assert!(!failed.stderr.contains("mock-signature"));
}

#[test]
fn storage_failures_are_retried_on_the_same_url() {
    let f = Fixture::new();
    f.registry.fail(Failure { method: "PUT", path: "/mock-s3/", status: 503, times: 2 });
    f.stage().assert_code(0);
    assert_eq!(f.registry.count("POST", "/upload-url"), 4);
    assert_eq!(f.registry.count("PUT", "/mock-s3/"), 6);

    let f = Fixture::new();
    f.registry.fail(Failure { method: "PUT", path: "/mock-s3/", status: 500, times: usize::MAX });
    let failed = f.stage_with(&["-j", "1"]);
    failed.assert_code(5);
    let puts: Vec<String> = f.registry.requests().iter().filter(|r| r.path().starts_with("/mock-s3/")).map(|r| r.url.clone()).collect();
    assert_eq!(puts.len(), 4, "four attempts");
    assert!(puts.iter().all(|u| *u == puts[0]), "retries reuse the URL");
    assert_eq!(f.registry.count("POST", "/upload-url"), 1);
    assert!(failed.stderr.contains("object storage failed with HTTP 500"), "{}", failed.stderr);
}

#[test]
fn asynchronous_verification_is_polled() {
    let f = Fixture::new();
    f.registry.with(|s| {
        s.async_ready = 1;
        s.async_polls = 2;
    });
    f.stage().assert_code(0);
    // Two "verifying" answers, the "ready" one and the final completeness check.
    assert_eq!(f.registry.count("GET", &format!("/releases/{RELEASE_ID}")), 4);
    assert_eq!(f.registry.with(|s| s.uploaded.len()), 4);

    let f = Fixture::new();
    f.registry.with(|s| s.async_failed = 1);
    let failed = f.stage_with(&["-j", "1"]);
    failed.assert_code(5);
    assert!(failed.stderr.contains("rejected the archive"), "{}", failed.stderr);
    assert!(!failed.stderr.contains("differs from manifest"), "server messages are not printed");
    let status = run(f.cmd(&["status"]));
    status.assert_code(0);
    assert!(status.stdout().contains("0 of 4 ready, 1 rejected"), "{}", status.stdout());
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "needs the debug-only PPR_TOOL_TEST_VERIFY_TIMEOUT_MS hook")]
fn verification_timeout_is_bounded() {
    let f = Fixture::new();
    f.registry.with(|s| {
        s.async_ready = 1;
        s.async_polls = usize::MAX;
    });
    let args = f.stage_args();
    let failed = run(f.cmd(&[&args.iter().map(String::as_str).collect::<Vec<_>>()[..], &["-j", "1"]].concat()).env("PPR_TOOL_TEST_VERIFY_TIMEOUT_MS", "100"));
    failed.assert_code(5);
    assert!(failed.stderr.contains("timed out waiting for the registry to verify"), "{}", failed.stderr);
}

#[test]
fn artifacts_verified_elsewhere_are_not_uploaded_again() {
    let f = Fixture::new();
    f.registry.with(|s| s.verifying_elsewhere = 1);
    let staged = f.stage();
    staged.assert_code(0);
    assert!(staged.stderr.contains("3 uploaded, 1 already present"), "{}", staged.stderr);
    assert_eq!(f.registry.count("PUT", "/mock-s3/"), 3);

    // A verification elsewhere that ends without a verdict: upload the archive ourselves.
    let f = Fixture::new();
    f.registry.with(|s| s.abandoned_elsewhere = 1);
    let staged = f.stage_with(&["-j", "1"]);
    staged.assert_code(0);
    assert!(staged.stderr.contains("4 uploaded, 0 already present"), "{}", staged.stderr);
    assert_eq!(f.registry.count("POST", "/upload-url"), 5);
    assert_eq!(f.registry.count("PUT", "/mock-s3/"), 4);

    // ... but only once per archive.
    let f = Fixture::new();
    f.registry.with(|s| s.abandoned_elsewhere = 2);
    let failed = f.stage_with(&["-j", "1"]);
    failed.assert_code(5);
    assert!(failed.stderr.contains("did not finish"), "{}", failed.stderr);
    assert_eq!(f.registry.count("PUT", "/mock-s3/"), 0);
}

#[test]
fn stale_uploads() {
    // Another process finished the artifact with a newer URL: nothing left to do.
    let f = Fixture::new();
    f.registry.with(|s| s.completed_elsewhere = 1);
    f.stage().assert_code(0);

    let f = Fixture::new();
    f.registry.with(|s| s.stale_uploads = 1);
    let failed = f.stage_with(&["--json", "-j", "1"]);
    failed.assert_code(5);
    let error = &failed.json()["error"];
    assert_eq!(error["code"], "stale_upload");
    assert!(error["hint"].as_str().unwrap().contains("another ppr-tool process"));
}

#[test]
fn unsafe_upload_tickets_are_refused() {
    // (upload URL override, extra signed header)
    type Case = (Option<&'static str>, Option<(&'static str, &'static str)>);
    let cases: [Case; 5] = [
        (Some("http://storage.example.com/bucket/key?X-Amz-Signature=leaked"), None),
        (Some("ftp://127.0.0.1/bucket/key"), None),
        (Some("https://user:leaked@storage.example.com/bucket/key"), None),
        (None, Some(("Authorization", "Bearer leaked"))),
        (None, Some(("Cookie", "session=leaked"))),
    ];
    for (url, header) in cases {
        let f = Fixture::new();
        f.registry.with(|s| {
            s.ticket_url = url.map(str::to_string);
            s.ticket_headers = header.map(|(n, v)| vec![(n.to_string(), v.to_string())]).unwrap_or_default();
        });
        let failed = f.stage_with(&["-j", "1"]);
        failed.assert_code(5);
        assert_eq!(f.registry.count("PUT", "/mock-s3/"), 0);
        assert!(!failed.stderr.contains("leaked"), "{}", failed.stderr);
    }
}
