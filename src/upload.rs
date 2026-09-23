//! Parallel artifact uploads with progress reporting.

use crate::actions;
use crate::canonical::hex;
use crate::error::{Error, Result};
use crate::http::{Body, Client, route_for};
use crate::output::{Ui, human_size};
use crate::release::{Package, Prepared};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub struct Outcome {
    pub uploaded: usize,
    pub skipped: usize,
}

fn file_sha256(package: &Package) -> Result<String> {
    let mut file = std::fs::File::open(&package.path).map_err(|e| Error::validation(format!("{}: cannot read archive: {e}", package.display)))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer).map_err(|e| Error::validation(format!("{}: cannot read archive: {e}", package.display)))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Streams one archive from disk; memory use stays flat regardless of archive size or job count.
fn upload_one(client: &Client, prepared: &Prepared, release_id: &str, package: &Package, bar: Option<&ProgressBar>) -> Result<()> {
    if file_sha256(package)? != package.sha256 {
        return Err(Error::validation(format!("{}: artifact changed after validation", package.display))
            .hint("never rebuild artifacts while a release is staged; rerun stage from scratch"));
    }
    let route = format!("{}/artifacts/{}", route_for(&prepared.scope.product, Some(release_id)), package.key);
    let idempotency = format!("{}:{}", prepared.digest, package.key);
    let reported = AtomicU64::new(0);
    let progress = |sent: u64| {
        let Some(bar) = bar else { return };
        let previous = reported.swap(sent, Ordering::Relaxed);
        // A retry starts over from zero, so the bar may move back.
        if sent >= previous { bar.inc(sent - previous) } else { bar.dec(previous - sent) }
    };
    let body = Body::File { path: &package.path, size: package.size, progress: &progress };
    client.request("PUT", &route, body, &[("x-content-sha256", &package.sha256), ("idempotency-key", &idempotency)])?;
    Ok(())
}

/// Uploads every package not already accepted by the registry, `jobs` at a time.
pub fn upload_all(client: &Client, prepared: &Prepared, release_id: &str, already: &HashSet<String>, jobs: usize, ui: &Ui) -> Result<Outcome> {
    let pending: Vec<&Package> = prepared.packages.iter().filter(|p| !already.contains(&p.key)).collect();
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
                        Ok(()) => {
                            *uploaded.lock().unwrap() += 1;
                            let line = format!("Uploaded {} {}@{} ({})", package.format, package.name, package.version, human_size(package.size));
                            match &progress {
                                Some(bar) => bar.println(if ui.stderr_color() { ui.green("✓ ") + &line } else { format!("✓ {line}") }),
                                None => ui.info(&line),
                            }
                        }
                        Err(error) => {
                            stop.store(true, Ordering::SeqCst);
                            let mut slot = failure.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(error);
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
    Ok(Outcome { uploaded: uploaded.into_inner().unwrap(), skipped })
}
