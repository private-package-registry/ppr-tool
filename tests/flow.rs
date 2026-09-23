//! End-to-end publication against a mock registry: stage, verify, commit, status and publish.

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

    /// A command with the static token and registry configured.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut command = tool(self.dir.path());
        command.args(args).env("PPR_TOKEN", STATIC_TOKEN).env("PPR_REGISTRY", &self.registry.origin);
        command
    }

    fn stage_args(&self) -> Vec<String> {
        let mut args: Vec<String> = ["stage"].iter().chain(scope_args().iter()).map(|s| s.to_string()).collect();
        args.extend(self.files.iter().cloned());
        args
    }

    fn stage(&self) -> Output {
        let args = self.stage_args();
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
    assert_eq!(f.registry.count("PUT", "/artifacts/"), 4);
    let state_mock = f.registry.state.lock().unwrap().uploaded.len();
    assert_eq!(state_mock, 4);

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
    assert_eq!(f.registry.count("PUT", "/artifacts/"), 4);
    // The registry reports every package as uploaded; a second stage only re-checks the status.
    let again = f.stage();
    again.assert_code(0);
    assert_eq!(f.registry.count("PUT", "/artifacts/"), 4);
    assert!(again.stderr.contains("0 uploaded, 4 already present"));
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
    f.registry.fail(Failure { method: "PUT", path: "/artifacts/", status: 422, times: usize::MAX });
    f.stage().assert_code(5);

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
    let exchange: Value = serde_json::from_slice(&requests.iter().find(|r| r.path() == "/api/v1/auth/oidc").unwrap().body).unwrap();
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
fn oidc_failures_are_auth_errors() {
    let f = Fixture::new();
    f.registry.fail(Failure { method: "POST", path: "/api/v1/auth/oidc", status: 400, times: usize::MAX });
    let args = f.stage_args();
    let mut stage = f.cmd(&args.iter().map(String::as_str).collect::<Vec<_>>());
    oidc(&f, &mut stage);
    run(&mut stage).assert_code(4);

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
