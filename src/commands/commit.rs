use super::{Context, fetch_status, open_session, reprepare};
use crate::cli::{RegistryArgs, ReleaseArgs, StateArgs};
use crate::error::{Error, Result};
use crate::http::{Body, Client, route_for};
use crate::state::State;
use serde_json::json;

pub fn run(ctx: &Context, state_args: &StateArgs, release: &ReleaseArgs, registry: &RegistryArgs, json: bool) -> Result<()> {
    let (_path, state_value, client) = open_session(ctx, state_args, release, registry)?;
    run_with_state(ctx, &state_value, &client, json)
}

pub fn run_with_state(ctx: &Context, state_value: &State, client: &Client, json: bool) -> Result<()> {
    let ui = &ctx.ui;
    let prepared = reprepare(state_value)?;
    if !state_value.tested() {
        return Err(Error::state("release has not been verified").hint("run ppr-tool verify -- COMMAND before commit"));
    }
    let status = fetch_status(client, &state_value.product, &state_value.release_id)?;
    if status.digest != prepared.digest || !status.complete {
        return Err(Error::registry("registry release does not match the verified artifacts").hint("rerun ppr-tool stage to resume the draft"));
    }
    let body = serde_json::to_vec(&json!({ "manifestSha256": prepared.digest }))?;
    let idempotency = format!("commit:{}", prepared.digest);
    client.request(
        "POST",
        &format!("{}/commit", route_for(&state_value.product, Some(&state_value.release_id))),
        Body::Json(&body),
        &[("idempotency-key", &idempotency)],
    )?;
    if json {
        super::print_json(
            &json!({ "releaseId": state_value.release_id, "product": state_value.product, "version": state_value.version, "variant": state_value.variant, "digest": prepared.digest, "published": true }),
        );
    } else {
        ui.success(&format!("Published {} {} ({}), release {}", state_value.product, state_value.version, state_value.variant, state_value.release_id));
    }
    Ok(())
}
