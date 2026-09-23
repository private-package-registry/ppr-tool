use super::Context;
use crate::cli::{ArchiveArgs, ScopeArgs};
use crate::error::Result;
use crate::inputs::{self, Scope};
use crate::release;
use std::io::Write;

pub fn run(ctx: &Context, scope: &ScopeArgs, archives: &ArchiveArgs, json: bool) -> Result<()> {
    let scope_value = Scope::resolve(scope.product.clone(), scope.variant.clone(), scope.commit.clone())?;
    let inputs = inputs::expand(&archives.archives)?;
    let prepared = release::prepare(&scope_value, &inputs, scope.expect_version.as_deref())?;
    if json {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&prepared.wire)?;
        stdout.flush()?;
        return Ok(());
    }
    let ui = &ctx.ui;
    ui.out(&format!(
        "{} {} {} ({}) from commit {}",
        ui.bold("Release"),
        prepared.scope.product,
        prepared.version,
        prepared.scope.variant,
        &prepared.scope.commit[..12]
    ));
    ui.out("");
    ui.out(ui.table(&super::package_rows(&prepared)).trim_end());
    ui.out("");
    ui.out(&format!(
        "{} {} package{}; manifest SHA-256 {}",
        ui.green("Valid:"),
        prepared.packages.len(),
        if prepared.packages.len() == 1 { "" } else { "s" },
        prepared.digest
    ));
    Ok(())
}
