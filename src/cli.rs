//! Command-line grammar.

use crate::output::ColorMode;
use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "ppr-tool", version, about = "Publish multi-ecosystem releases to a private package registry.", long_about = None, disable_help_subcommand = true, max_term_width = 100)]
pub struct Cli {
    /// When to use colours on stderr
    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto, value_name = "WHEN")]
    pub color: ColorMode,
    /// Suppress progress and informational messages
    #[arg(short, long, global = true)]
    pub quiet: bool,
    #[command(subcommand)]
    pub command: Command,
}

/// Release identity.
#[derive(Args, Debug, Clone)]
pub struct ScopeArgs {
    /// Product slug (env: PPR_PRODUCT)
    #[arg(long, env = "PPR_PRODUCT", value_name = "NAME", hide_env_values = true)]
    pub product: Option<String>,
    /// Release variant slug, for example sources (env: PPR_VARIANT)
    #[arg(long, env = "PPR_VARIANT", value_name = "NAME", hide_env_values = true)]
    pub variant: Option<String>,
    /// Source commit SHA; defaults to GITHUB_SHA on GitHub Actions (env: PPR_COMMIT)
    #[arg(long, env = "PPR_COMMIT", value_name = "SHA", hide_env_values = true)]
    pub commit: Option<String>,
    /// Expected release version; fails when the archives carry a different one
    #[arg(long = "version", value_name = "X.Y.Z")]
    pub expect_version: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ArchiveArgs {
    /// Package archives: .tgz/.tar.gz (npm), .crate (cargo), .nupkg (nuget), .zip (composer). Globs are expanded by ppr-tool.
    #[arg(value_name = "ARCHIVE", required = true)]
    pub archives: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct RegistryArgs {
    /// Registry origin, for example https://registry.example.com (env: PPR_REGISTRY)
    #[arg(long, env = "PPR_REGISTRY", value_name = "URL", hide_env_values = true)]
    pub registry: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct StateArgs {
    /// Publication state file (env: PPR_STATE) [default: .ppr-tool/state.json]
    #[arg(long, env = "PPR_STATE", value_name = "FILE", hide_env_values = true)]
    pub state: Option<std::path::PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct ReleaseArgs {
    /// Release ID to operate on; must match the state file [default: from state]
    #[arg(long, value_name = "ID")]
    pub release: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct JobsArgs {
    /// Parallel uploads
    #[arg(short, long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=16), value_name = "N")]
    pub jobs: u8,
}

/// Everything `stage` needs; shared with `publish`.
#[derive(Args, Debug, Clone)]
pub struct StageArgs {
    #[command(flatten)]
    pub scope: ScopeArgs,
    #[command(flatten)]
    pub archives: ArchiveArgs,
    #[command(flatten)]
    pub registry: RegistryArgs,
    #[command(flatten)]
    pub state: StateArgs,
    #[command(flatten)]
    pub jobs: JobsArgs,
}

#[derive(Args, Debug, Clone)]
pub struct VerifyCommandArgs {
    /// Command that installs the draft packages; receives PPR_PREVIEW, PPR_PREVIEW_URL, PPR_RELEASE_ID and PPR_DOWNLOAD_TOKEN
    #[arg(last = true, value_name = "COMMAND", required = true, num_args = 1..)]
    pub command: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Inspect archives and print the release that would be staged; no network access
    Validate {
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        archives: ArchiveArgs,
        /// Print the exact wire manifest bytes instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Create or resume a draft release and upload its archives
    Stage {
        #[command(flatten)]
        stage: StageArgs,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Run an installation test against the draft and record the result locally
    Verify {
        #[command(flatten)]
        state: StateArgs,
        #[command(flatten)]
        release: ReleaseArgs,
        #[command(flatten)]
        registry: RegistryArgs,
        #[command(flatten)]
        verify: VerifyCommandArgs,
    },
    /// Publish a verified draft release
    Commit {
        #[command(flatten)]
        state: StateArgs,
        #[command(flatten)]
        release: ReleaseArgs,
        #[command(flatten)]
        registry: RegistryArgs,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show the local publication state and the registry's view of the draft
    Status {
        #[command(flatten)]
        state: StateArgs,
        #[command(flatten)]
        release: ReleaseArgs,
        #[command(flatten)]
        registry: RegistryArgs,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Stage, verify and commit in one go
    Publish {
        #[command(flatten)]
        stage: StageArgs,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        verify: VerifyCommandArgs,
    },
}
