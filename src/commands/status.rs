use super::{Context, fetch_status, open_session};
use crate::cli::{RegistryArgs, ReleaseArgs, StateArgs};
use crate::error::Result;
use crate::time;
use serde_json::json;

pub fn run(ctx: &Context, state_args: &StateArgs, release: &ReleaseArgs, registry: &RegistryArgs, json: bool) -> Result<()> {
    let (path, state_value, client) = open_session(ctx, state_args, release, registry)?;
    let remote = fetch_status(&client, &state_value.product, &state_value.release_id)?;
    let expires_in = state_value.expires_in();
    let session = match (state_value.expires_at.as_deref(), expires_in) {
        (Some(at), Some(seconds)) => format!(
            "{at} ({})",
            if seconds >= 0 { format!("in {}", time::human_duration(seconds)) } else { format!("expired {}", time::human_duration(seconds)) }
        ),
        _ => "static token (no expiry)".to_string(),
    };
    if json {
        super::print_json(&json!({
            "releaseId": state_value.release_id,
            "registry": state_value.registry,
            "product": state_value.product,
            "version": state_value.version,
            "variant": state_value.variant,
            "commit": state_value.commit,
            "preview": state_value.preview,
            "digest": state_value.digest,
            "archives": state_value.archives,
            "complete": remote.complete,
            "digestMatches": remote.digest == state_value.digest,
            "tested": state_value.tested(),
            "expiresAt": state_value.expires_at,
            "createdAt": state_value.created_at,
            "state": path.to_string_lossy(),
        }));
        return Ok(());
    }
    let ui = &ctx.ui;
    let rows = vec![
        vec!["Release".to_string(), state_value.release_id.clone()],
        vec!["Product".to_string(), format!("{} {} ({})", state_value.product, state_value.version, state_value.variant)],
        vec!["Commit".to_string(), state_value.commit.clone()],
        vec!["Registry".to_string(), state_value.registry.clone()],
        vec!["Preview".to_string(), state_value.preview.clone()],
        vec!["Archives".to_string(), state_value.archives.len().to_string()],
        vec!["Uploads".to_string(), if remote.complete { ui.green("complete") } else { ui.yellow("incomplete") }],
        vec![
            "Digest".to_string(),
            if remote.digest == state_value.digest { format!("{} (matches registry)", &state_value.digest[..12]) } else { ui.red("differs from registry") },
        ],
        vec!["Verified".to_string(), if state_value.tested() { ui.green("yes") } else { ui.yellow("no") }],
        vec!["Session".to_string(), session],
        vec!["State file".to_string(), path.to_string_lossy().into_owned()],
    ];
    ui.out(ui.table(&rows).trim_end());
    Ok(())
}
