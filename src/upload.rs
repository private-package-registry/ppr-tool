//! Parallel artifact uploads with progress reporting: upload URL, presigned PUT, completion.

use crate::actions;
use crate::canonical::hex;
use crate::error::{Error, Result};
use crate::http::{ATTEMPTS, Client, PutOutcome, STORAGE_HINT};
use crate::output::{Ui, human_size};
use crate::protocol::{self, Completion, Issued};
use crate::release::{Package, Prepared};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Upper bound for asynchronous verification (HTTP 202) before giving up; `stage` can resume later.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(600);
const REJECTED_HINT: &str = "the archive does not match the manifest, the product catalog or the registry's archive rules; check the registry's audit log";

pub struct Outcome {
    pub uploaded: usize,
    pub skipped: usize,
}

enum Uploaded {
    Sent,
    /// The registry had already verified the artifact (for example after a lost response).
    AlreadyPresent,
}

fn file_sha256(package: &Package) -> Result<String> {
    let mut file = std::fs::File::open(&package.path).map_err(|e| Error::validation(format!("cannot read archive: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).map_err(|e| Error::validation(format!("cannot read archive: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Debug builds only: lets the integration tests poll quickly and time out early.
fn test_duration(variable: &str, default: Duration) -> Duration {
    if cfg!(debug_assertions)
        && let Some(ms) = std::env::var(variable).ok().and_then(|v| v.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    default
}

fn rejected() -> Error {
    Error::registry("the registry rejected the archive").hint(REJECTED_HINT)
}

/// Polls the release status while an artifact is `verifying`. `Ok(true)`: ready; `Ok(false)`: the
/// verification ended without a verdict (the artifact went back to `uploading` or `declared`).
fn wait_for_verification(client: &Client, product: &str, release_id: &str, key: &str) -> Result<bool> {
    let deadline = Instant::now() + test_duration("PPR_TOOL_TEST_VERIFY_TIMEOUT_MS", VERIFY_TIMEOUT);
    loop {
        std::thread::sleep(test_duration("PPR_TOOL_TEST_POLL_MS", Duration::from_secs(2)));
        let status = protocol::fetch_status(client, product, release_id)?;
        match status.state_of(key) {
            Some("ready") => return Ok(true),
            Some("failed") => return Err(rejected()),
            Some("verifying") if Instant::now() < deadline => continue,
            Some("verifying") => {
                return Err(Error::registry("timed out waiting for the registry to verify the archive").hint("rerun ppr-tool stage later to resume"));
            }
            _ => return Ok(false),
        }
    }
}

fn unfinished() -> Error {
    Error::registry("archive verification did not finish").hint("rerun ppr-tool stage to upload it again")
}

/// Uploads one archive: presigned URL, PUT straight to storage (streamed from disk, so memory stays flat
/// regardless of archive size or job count), then completion. A missing object (412) and an expired
/// URL (403 from storage, or a connection storage dropped on every attempt) each earn one fresh URL,
/// and so does a verification running elsewhere that ends without a verdict.
fn upload_one(client: &Client, prepared: &Prepared, release_id: &str, package: &Package, bar: Option<&ProgressBar>) -> Result<Uploaded> {
    if file_sha256(package)? != package.sha256 {
        return Err(Error::validation("artifact changed after validation").hint("never rebuild artifacts while a release is staged; rerun stage from scratch"));
    }
    let product = &prepared.scope.product;
    let route = protocol::artifact_route(product, release_id, &package.key);
    let idempotency = format!("{}:{}", prepared.digest, package.key);
    let reported = AtomicU64::new(0);
    let progress = |sent: u64| {
        let Some(bar) = bar else { return };
        let previous = reported.swap(sent, Ordering::Relaxed);
        // A retry starts over from zero, so the bar may move back.
        if sent >= previous { bar.inc(sent - previous) } else { bar.dec(previous - sent) }
    };
    let (mut renewed_missing, mut renewed_expired, mut waited) = (false, false, false);
    loop {
        let ticket = match protocol::request_upload(client, &route, &idempotency) {
            Ok(Issued::Upload(ticket)) => ticket,
            Ok(Issued::Ready) => {
                progress(package.size);
                return Ok(Uploaded::AlreadyPresent);
            }
            // Another request is still verifying this artifact.
            Err(error) if error.http_status() == Some(409) && error.code() == Some("verifying") => {
                if wait_for_verification(client, product, release_id, &package.key)? {
                    progress(package.size);
                    return Ok(Uploaded::AlreadyPresent);
                }
                if waited {
                    return Err(unfinished());
                }
                // That verification ended without a verdict; upload the archive ourselves.
                waited = true;
                continue;
            }
            Err(error) => return Err(artifact_error(error)),
        };
        match client.put_presigned(&ticket.url, &package.path, package.size, &ticket.headers, &progress)? {
            PutOutcome::Stored => {}
            PutOutcome::Rejected(403) | PutOutcome::Interrupted(_) if !renewed_expired => {
                renewed_expired = true;
                continue;
            }
            PutOutcome::Interrupted(message) => {
                return Err(Error::registry(format!("upload to object storage failed: {message} (after {ATTEMPTS} attempts)")).hint(STORAGE_HINT));
            }
            PutOutcome::Rejected(status) => {
                let hint = if status == 403 {
                    "the upload URL was refused twice; check the system clock and the network, then rerun ppr-tool stage"
                } else {
                    "rerun ppr-tool stage; if it persists, the registry's storage configuration is broken"
                };
                return Err(Error::registry(format!("object storage refused the upload with HTTP {status}"))
                    .hint(hint)
                    .detail(serde_json::json!({ "storageStatus": status })));
            }
        }
        match protocol::complete_upload(client, &route, &idempotency, &ticket.upload_id) {
            Ok(Completion::Ready) => return Ok(Uploaded::Sent),
            Ok(Completion::Verifying) => {
                return if wait_for_verification(client, product, release_id, &package.key)? { Ok(Uploaded::Sent) } else { Err(unfinished()) };
            }
            Err(error) if error.http_status() == Some(412) && !renewed_missing => renewed_missing = true,
            // A newer URL superseded ours; fine if whoever holds it finished the job.
            Err(error) if error.code() == Some("stale_upload") => {
                if protocol::fetch_status(client, product, release_id)?.state_of(&package.key) == Some("ready") {
                    return Ok(Uploaded::Sent);
                }
                return Err(error);
            }
            Err(error) => return Err(artifact_error(error)),
        }
    }
}

/// Artifact routes answer 409 when the release is no longer a draft; the generic stage hint would mislead.
fn artifact_error(mut error: Error) -> Error {
    match error.http_status() {
        Some(409) if error.code() != Some("stale_upload") => {
            error.hint = Some("the release is no longer a draft or changed on the registry; run ppr-tool status".to_string());
        }
        Some(422) => error.hint = Some(REJECTED_HINT.to_string()),
        _ => {}
    }
    error
}

/// Uploads every package the registry reported as pending, `jobs` at a time.
pub fn upload_all(client: &Client, prepared: &Prepared, release_id: &str, pending_keys: &HashSet<String>, jobs: usize, ui: &Ui) -> Result<Outcome> {
    let pending: Vec<&Package> = prepared.packages.iter().filter(|p| pending_keys.contains(&p.key)).collect();
    let skipped = prepared.packages.len() - pending.len();
    if pending.is_empty() {
        return Ok(Outcome { uploaded: 0, skipped });
    }
    let total_bytes: u64 = pending.iter().map(|p| p.size).sum();
    ui.info(&format!(
        "Uploading {} package{} ({}){}",
        pending.len(),
        if pending.len() == 1 { "" } else { "s" },
        human_size(total_bytes),
        if skipped > 0 { format!(", {skipped} already uploaded") } else { String::new() }
    ));
    actions::group(&format!("Uploading {} packages", pending.len()));
    let progress = if ui.is_tty() && !ui.quiet() {
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
        let bar = multi.add(ProgressBar::new(total_bytes));
        bar.set_style(ProgressStyle::with_template("{bar:30.cyan/dim} {bytes}/{total_bytes} {msg}").unwrap().progress_chars("=> "));
        Some(bar)
    } else {
        None
    };
    let queue = Mutex::new(pending.clone());
    let failure: Mutex<Option<Error>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    let uploaded = Mutex::new(0usize);
    let present = Mutex::new(0usize);
    std::thread::scope(|scope| {
        for _ in 0..jobs.clamp(1, pending.len()) {
            scope.spawn(|| {
                loop {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let next = queue.lock().unwrap().pop();
                    let Some(package) = next else { break };
                    if let Some(bar) = &progress {
                        bar.set_message(format!("{} {}", package.format, package.name));
                    }
                    match upload_one(client, prepared, release_id, package, progress.as_ref()) {
                        Ok(result) => {
                            let verb = match result {
                                Uploaded::Sent => {
                                    *uploaded.lock().unwrap() += 1;
                                    "Uploaded"
                                }
                                Uploaded::AlreadyPresent => {
                                    *present.lock().unwrap() += 1;
                                    "Already present:"
                                }
                            };
                            let line = format!("{verb} {} {}@{} ({})", package.format, package.name, package.version, human_size(package.size));
                            match &progress {
                                Some(bar) => bar.println(if ui.stderr_color() { ui.green("✓ ") + &line } else { format!("✓ {line}") }),
                                None => ui.info(&line),
                            }
                        }
                        Err(error) => {
                            stop.store(true, Ordering::SeqCst);
                            let mut slot = failure.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(error.at(&package.display));
                            }
                            break;
                        }
                    }
                }
            });
        }
    });
    if let Some(bar) = progress {
        bar.finish_and_clear();
    }
    actions::end_group();
    if let Some(error) = failure.into_inner().unwrap() {
        return Err(error);
    }
    Ok(Outcome { uploaded: uploaded.into_inner().unwrap(), skipped: skipped + present.into_inner().unwrap() })
}
