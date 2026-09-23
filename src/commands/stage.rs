use super::{Context, fetch_status, package_json, package_rows};
use crate::actions;
use crate::auth;
use crate::cli::StageArgs;
use crate::error::{Error, Result};
use crate::http::{Body, Client, route_for};
use crate::inputs::{self, Scope};
use crate::names;
use crate::release::{self, Prepared};
use crate::state::{self, State};
use crate::time;
use crate::upload;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;

pub struct Staged {
    pub path: PathBuf,
    pub state: State,
    pub client: Client,
    pub prepared: Prepared,
    pub uploaded: usize,
    pub skipped: usize,
}

pub fn run(ctx: &Context, args: &StageArgs, json: bool) -> Result<()> {
    let staged = stage(ctx, args)?;
    let ui = &ctx.ui;
    let (prepared, release_id, preview) = (&staged.prepared, &staged.state.release_id, &staged.state.preview);
    if json {
        super::print_json(&json!({
            "releaseId": release_id,
            "preview": preview,
            "digest": prepared.digest,
            "product": prepared.scope.product,
            "version": prepared.version,
            "variant": prepared.scope.variant,
            "commit": prepared.scope.commit,
            "uploaded": staged.uploaded,
            "skipped": staged.skipped,
            "state": staged.path.to_string_lossy(),
            "packages": package_json(prepared),
        }));
    } else {
        ui.out(&format!("Release ID: {release_id}"));
        ui.out(&format!("Preview:    {preview}"));
        ui.info(&format!("Next: ppr-tool verify -- COMMAND, then ppr-tool commit (state: {})", staged.path.display()));
    }
    Ok(())
}

/// Prepares, reserves and uploads the release; reports progress on stderr only.
pub fn stage(ctx: &Context, args: &StageArgs) -> Result<Staged> {
    let ui = &ctx.ui;
    let (scope, archives, registry, state_args, jobs) = (&args.scope, &args.archives, &args.registry, &args.state, &args.jobs);
    let scope_value = Scope::resolve(scope.product.clone(), scope.variant.clone(), scope.commit.clone())?;
    let registry_origin = registry.registry.clone().unwrap_or_default();
    let mut client = Client::new(&registry_origin)?;
    let inputs = inputs::expand(&archives.archives)?;
    ui.info(&format!("Inspecting {} archive{}", inputs.len(), if inputs.len() == 1 { "" } else { "s" }));
    let prepared = release::prepare(&scope_value, &inputs, scope.expect_version.as_deref())?;
    ui.info(&format!(
        "Release {} {} ({}) — {} packages, manifest {}",
        prepared.scope.product,
        prepared.version,
        prepared.scope.variant,
        prepared.packages.len(),
        &prepared.digest[..12]
    ));
    let session = auth::authenticate(&client, &scope_value, &prepared.version, ui)?;
    client.set_token(Some(session.token.clone()));
    let staged = client.request("POST", &route_for(&scope_value.product, None), Body::Json(&prepared.wire), &[("idempotency-key", &prepared.digest)])?;
    let release_id = staged
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| names::is_release_id(id))
        .ok_or_else(|| Error::registry("registry returned an invalid release identity"))?
        .to_string();
    if staged.get("manifestSha256").and_then(Value::as_str) != Some(prepared.digest.as_str()) {
        return Err(Error::registry("registry returned a different release identity").hint("the registry's manifest digest does not match the local one"));
    }
    let path = super::state_path(state_args);
    let preview = format!("{}/preview/{release_id}", client.registry);
    let state_value = State {
        schema_version: 2,
        registry: client.registry.clone(),
        release_id: release_id.clone(),
        product: scope_value.product.clone(),
        version: prepared.version.clone(),
        variant: scope_value.variant.clone(),
        commit: scope_value.commit.clone(),
        digest: prepared.digest.clone(),
        archives: prepared.packages.iter().map(|p| p.path.clone()).collect(),
        token: session.token.clone(),
        expires_at: session.expires_at.clone(),
        preview: preview.clone(),
        tested_digest: None,
        created_at: time::format_rfc3339(time::now()),
    };
    state::save(&path, &state_value)?;
    let already: HashSet<String> =
        staged.get("uploaded").and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let outcome = upload::upload_all(&client, &prepared, &release_id, &already, jobs.jobs as usize, ui)?;
    let status = fetch_status(&client, &scope_value.product, &release_id)?;
    if status.digest != prepared.digest || !status.complete {
        return Err(Error::registry("release is incomplete after uploads").hint("rerun ppr-tool stage to resume"));
    }
    actions::set_output("release-id", &release_id)?;
    actions::set_output("preview-url", &preview)?;
    actions::export_env("PPR_RELEASE_ID", &release_id)?;
    if actions::active() {
        let mut summary = format!(
            "### ppr-tool staged {} {} ({})\n\nRelease `{}` · preview {}\n\n| Format | Package | Version | Size | SHA-256 |\n|---|---|---|---|---|\n",
            prepared.scope.product, prepared.version, prepared.scope.variant, release_id, preview
        );
        for package in &prepared.packages {
            summary.push_str(&format!(
                "| {} | `{}` | {} | {} | `{}…` |\n",
                package.format,
                package.name,
                package.version,
                crate::output::human_size(package.size),
                &package.sha256[..12]
            ));
        }
        summary.push('\n');
        actions::step_summary(&summary)?;
    }
    if !ui.quiet() {
        ui.info("");
        ui.info(ui.table(&package_rows(&prepared)).trim_end());
        ui.info("");
    }
    ui.success(&format!("Staged release {release_id} ({} uploaded, {} already present)", outcome.uploaded, outcome.skipped));
    Ok(Staged { path, state: state_value, client, prepared, uploaded: outcome.uploaded, skipped: outcome.skipped })
}
