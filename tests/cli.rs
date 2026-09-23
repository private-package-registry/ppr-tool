//! Offline behaviour of the binary: grammar, exit codes, validation and the wire manifest.

mod support;

use serde_json::Value;
use support::*;

fn validate(dir: &TempDir, extra: &[&str]) -> Output {
    let mut args = vec!["validate"];
    args.extend(scope_args());
    args.extend_from_slice(extra);
    ppr(dir.path(), &args)
}

#[test]
fn prints_version_and_help() {
    let dir = TempDir::new();
    let version = ppr(dir.path(), &["--version"]);
    version.assert_code(0);
    assert_eq!(version.stdout(), format!("ppr-tool {}\n", env!("CARGO_PKG_VERSION")));
    let help = ppr(dir.path(), &["--help"]);
    help.assert_code(0);
    for command in ["validate", "stage", "verify", "commit", "status", "publish"] {
        assert!(help.stdout().contains(command), "help lists {command}");
    }
    ppr(dir.path(), &["stage", "--help"]).assert_code(0);
}

#[test]
fn usage_errors_exit_2() {
    let dir = TempDir::new();
    dir.write("a.tgz", &npm("demo", "1.0.0"));
    ppr(dir.path(), &["verify"]).assert_code(2);
    ppr(dir.path(), &["nonsense"]).assert_code(2);
    ppr(dir.path(), &["validate", "--variant", "sources", "--commit", COMMIT, "a.tgz"]).assert_code(2);
    let missing_commit = ppr(dir.path(), &["validate", "--product", "demo", "--variant", "sources", "a.tgz"]);
    missing_commit.assert_code(2);
    assert!(missing_commit.stderr.contains("--commit"));
    ppr(dir.path(), &["validate", "--product", "Demo!", "--variant", "sources", "--commit", COMMIT, "a.tgz"]).assert_code(2);
    ppr(dir.path(), &["validate", "--product", "demo", "--variant", "sources", "--commit", "abc", "a.tgz"]).assert_code(2);
    ppr(dir.path(), &["stage", "--product", "demo", "--variant", "sources", "--commit", COMMIT, "-j", "0", "a.tgz"]).assert_code(2);
}

#[test]
fn scope_comes_from_the_environment_and_github_sha() {
    let dir = TempDir::new();
    dir.write("a.tgz", &npm("demo", "1.0.0"));
    let from_env =
        run(tool(dir.path()).args(["validate", "--json", "a.tgz"]).env("PPR_PRODUCT", "demo").env("PPR_VARIANT", "sources").env("PPR_COMMIT", COMMIT));
    from_env.assert_code(0);
    let on_actions = run(tool(dir.path())
        .args(["validate", "--json", "--product", "demo", "--variant", "sources", "a.tgz"])
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_SHA", COMMIT));
    on_actions.assert_code(0);
    assert_eq!(from_env.stdout, on_actions.stdout);
    assert_eq!(on_actions.json()["commit"], COMMIT);
}

#[test]
fn validates_a_multi_format_release() {
    let dir = TempDir::new();
    let files = write_release(&dir, "1.2.3");
    let refs: Vec<&str> = files.iter().map(String::as_str).collect();
    let table = validate(&dir, &refs);
    table.assert_code(0);
    let text = table.stdout();
    for name in ["@demo/sdk", "demo/sdk", "Demo.Sdk", "demo"] {
        assert!(text.contains(name), "table lists {name}:\n{text}");
    }
    assert!(!text.contains('\u{1b}'), "no colour codes when stdout is not a terminal");
    let digest = text.split("manifest SHA-256 ").nth(1).unwrap().trim().to_string();

    let json = validate(&dir, &[&["--json"], refs.as_slice()].concat());
    json.assert_code(0);
    assert_eq!(sha256_hex(&json.stdout), digest, "table digest is the SHA-256 of the --json bytes");

    let mut reversed = refs.clone();
    reversed.reverse();
    let again = validate(&dir, &[&["--json"], reversed.as_slice()].concat());
    assert_eq!(again.stdout, json.stdout, "argument order never changes the wire bytes");

    let manifest = json.json();
    assert_eq!(manifest["schemaVersion"], 1);
    assert_eq!(manifest["product"], "demo");
    assert_eq!(manifest["version"], "1.2.3");
    let packages = manifest["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 4);
    let keys: Vec<&str> = packages.iter().map(|p| p["key"].as_str().unwrap()).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "packages are sorted by key");
    let nuget = packages.iter().find(|p| p["format"] == "nuget").unwrap();
    assert_eq!(nuget["key"], sha256_hex(b"nuget:demo.sdk:1.2.3"), "NuGet identities are lowercased");
    assert_eq!(nuget["name"], "Demo.Sdk");
    assert_eq!(nuget["metadata"]["license"]["@_type"], "expression");
    let cargo = packages.iter().find(|p| p["format"] == "cargo").unwrap();
    assert_eq!(cargo["metadata"]["package"]["edition"], "2021");
    for package in packages {
        let object = package.as_object().unwrap();
        let mut fields: Vec<&str> = object.keys().map(String::as_str).collect();
        fields.sort();
        assert_eq!(fields, ["format", "key", "metadata", "name", "sha1", "sha256", "sha512", "size", "version"]);
        let bytes = std::fs::read(dir.path().join(files.iter().find(|f| f.starts_with(package["format"].as_str().unwrap())).unwrap())).unwrap();
        assert_eq!(package["sha256"], sha256_hex(&bytes));
        assert_eq!(package["size"], bytes.len());
        assert_eq!(package["sha512"].as_str().unwrap().len(), 88);
    }
    // JCS: sorted keys and no insignificant whitespace.
    let text = String::from_utf8(json.stdout.clone()).unwrap();
    assert!(text.starts_with("{\"commit\":"));
    assert!(!text.contains('\n'));
}

#[test]
fn expands_globs_and_deduplicates() {
    let dir = TempDir::new();
    write_release(&dir, "2.0.0");
    let globbed = validate(&dir, &["--json", "*/*.tgz", "*/*.crate", "nuget/*", "composer/*.zip", "npm/demo-2.0.0.tgz"]);
    globbed.assert_code(0);
    assert_eq!(globbed.json()["packages"].as_array().unwrap().len(), 4);
    let none = validate(&dir, &["missing/*.tgz"]);
    none.assert_code(3);
    assert!(none.stderr.contains("no files match"));
    validate(&dir, &["npm/nope.tgz"]).assert_code(3);
}

#[test]
fn local_validation_failures_exit_3() {
    let dir = TempDir::new();
    dir.write("a.tar", b"x");
    let extension = validate(&dir, &["a.tar"]);
    extension.assert_code(3);
    assert!(extension.stderr.contains("unsupported archive extension"));
    assert!(extension.stderr.contains("hint:"));

    dir.write("one.tgz", &npm("demo", "1.0.0"));
    dir.write("two.crate", &crate_archive("demo", "1.0.1"));
    let versions = validate(&dir, &["one.tgz", "two.crate"]);
    versions.assert_code(3);
    assert!(versions.stderr.contains("one.tgz → 1.0.0") && versions.stderr.contains("two.crate → 1.0.1"), "{}", versions.stderr);
    validate(&dir, &["--version", "9.9.9", "one.tgz"]).assert_code(3);

    dir.write("copy.tgz", &npm("demo", "1.0.0"));
    assert!(validate(&dir, &["one.tgz", "copy.tgz"]).assert_code(3).stderr.contains("duplicate package"));

    dir.write("private.tgz", &npm_json(r#"{"name":"secret","version":"1.0.0","private":true}"#));
    assert!(validate(&dir, &["private.tgz"]).assert_code(3).stderr.contains("private"));

    dir.write("local.tgz", &npm_json(r#"{"name":"local","version":"1.0.0","dependencies":{"x":"workspace:*"}}"#));
    assert!(validate(&dir, &["local.tgz"]).assert_code(3).stderr.contains("unresolved local dependency"));

    dir.write("prerelease.tgz", &npm("pre", "1.0.0-beta.1"));
    validate(&dir, &["prerelease.tgz"]).assert_code(3);

    let long = "a".repeat(215);
    dir.write("long.tgz", &npm(&long, "1.0.0"));
    validate(&dir, &["long.tgz"]).assert_code(3);

    dir.write(
        "Bad.Name.1.0.0.nupkg",
        &zip(&[ZipEntry::new("Bad.nuspec", b"<package><metadata><id x=\"1\">Bad</id><version>1.0.0</version></metadata></package>")]),
    );
    let nuspec = validate(&dir, &["Bad.Name.1.0.0.nupkg"]);
    nuspec.assert_code(3);
    assert!(nuspec.stderr.contains("Bad.Name.1.0.0.nupkg: Bad.nuspec: <id> must contain text only"), "{}", nuspec.stderr);

    dir.write("nopkg.crate", &tar_gz(&[("x-1.0.0/Cargo.toml", b"[workspace]\n")]));
    assert!(validate(&dir, &["nopkg.crate"]).assert_code(3).stderr.contains("[package]"));

    dir.write("empty.zip", b"");
    validate(&dir, &["empty.zip"]).assert_code(3);
}

#[test]
fn json_mode_reports_errors_as_json() {
    let dir = TempDir::new();
    dir.write("a.tar", b"x");
    let output = validate(&dir, &["--json", "a.tar"]);
    output.assert_code(3);
    let error = &output.json()["error"];
    assert_eq!(error["kind"], "validation");
    assert_eq!(error["exitCode"], 3);
    assert!(error["message"].as_str().unwrap().contains("a.tar"));
    assert!(matches!(error["hint"], Value::String(_)));
}

#[test]
fn github_annotations_go_to_stderr() {
    let dir = TempDir::new();
    dir.write("a.tar", b"x");
    let output = run(tool(dir.path())
        .args(["validate", "--json", "--product", "demo", "--variant", "sources", "a.tar"])
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_SHA", COMMIT));
    output.assert_code(3);
    assert!(output.stderr.contains("::error title=ppr-tool::"));
    output.json();
}
