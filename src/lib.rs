//! ppr-tool: publisher CLI for the private package registry.

pub mod actions;
pub mod archive;
pub mod auth;
pub mod canonical;
pub mod cli;
pub mod commands;
pub mod error;
pub mod http;
pub mod inputs;
pub mod metadata;
pub mod names;
pub mod output;
pub mod release;
pub mod state;
pub mod time;
pub mod upload;

use clap::Parser;
use cli::{Cli, Command};
use commands::Context;
use error::{Error, Kind};

/// Runs the CLI and returns the process exit code.
pub fn run<I: IntoIterator<Item = std::ffi::OsString>>(args: I) -> i32 {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => Kind::Usage.exit_code(),
            };
        }
    };
    let ctx = Context { ui: output::Ui::new(cli.color, cli.quiet) };
    let json = matches!(
        &cli.command,
        Command::Validate { json: true, .. }
            | Command::Stage { json: true, .. }
            | Command::Commit { json: true, .. }
            | Command::Status { json: true, .. }
            | Command::Publish { json: true, .. }
    );
    let result = match &cli.command {
        Command::Validate { scope, archives, json } => commands::validate::run(&ctx, scope, archives, *json),
        Command::Stage { stage, json } => commands::stage::run(&ctx, stage, *json),
        Command::Verify { state, release, registry, verify } => commands::verify::run(&ctx, state, release, registry, verify),
        Command::Commit { state, release, registry, json } => commands::commit::run(&ctx, state, release, registry, *json),
        Command::Status { state, release, registry, json } => commands::status::run(&ctx, state, release, registry, *json),
        Command::Publish { stage, json, verify } => commands::publish::run(&ctx, stage, *json, verify),
    };
    match result {
        Ok(()) => 0,
        Err(error) => report(&ctx, &error, json),
    }
}

fn report(ctx: &Context, error: &Error, json: bool) -> i32 {
    let message = ctx.ui.scrub(&error.message);
    let hint = error.hint.as_deref().map(|h| ctx.ui.scrub(h));
    ctx.ui.error(&message, hint.as_deref());
    actions::error_annotation("ppr-tool", &message);
    if json {
        let mut object = serde_json::json!({ "kind": error.kind.label(), "message": message, "hint": hint, "exitCode": error.kind.exit_code() });
        if let (Some(serde_json::Value::Object(extra)), Some(target)) = (&error.detail, object.as_object_mut()) {
            target.extend(extra.clone());
        }
        commands::print_json(&serde_json::json!({ "error": object }));
    }
    error.kind.exit_code()
}
