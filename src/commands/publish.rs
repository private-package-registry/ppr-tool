use super::{Context, commit, refresh_session, stage, verify};
use crate::cli::{StageArgs, VerifyCommandArgs};
use crate::error::Result;
use crate::state;
use serde_json::json;

pub fn run(ctx: &Context, args: &StageArgs, json: bool, verify_args: &VerifyCommandArgs) -> Result<()> {
    let mut staged = stage::stage(ctx, args)?;
    verify::run_with_state(ctx, &staged.path, staged.state.clone(), &verify_args.command, json)?;
    let mut current = state::load(&staged.path)?;
    // The verification command may outlive a short OIDC session.
    refresh_session(ctx, &staged.path, &mut current, &mut staged.client)?;
    commit::run_with_state(ctx, &current, &staged.client, false)?;
    if json {
        super::print_json(&json!({
            "releaseId": current.release_id,
            "preview": current.preview,
            "digest": current.digest,
            "product": current.product,
            "version": current.version,
            "variant": current.variant,
            "commit": current.commit,
            "uploaded": staged.uploaded,
            "skipped": staged.skipped,
            "published": true,
            "packages": super::package_json(&staged.prepared),
        }));
    }
    Ok(())
}
