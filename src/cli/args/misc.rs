//! Standalone verbs: version/upgrade/completions/probe/feedback.

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct VersionArgs {
    /// Query the binary on a client over SSH (server side)
    // flag-only, deliberately no default_client fallback: a bare
    // `shunt version` almost surely means THIS binary.
    #[arg(short, long)]
    pub client: Option<String>,
    /// Emit JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct UpgradeArgs {
    /// Upgrade the binary ON A CLIENT over SSH instead of the local
    /// one (always the musl static asset — runs on any libc)
    #[arg(short, long)]
    pub client: Option<String>,
    /// Report latest vs current without installing anything
    #[arg(long)]
    pub check: bool,
    /// Reinstall even when already at the latest release
    #[arg(long)]
    pub force: bool,
    /// Emit JSON
    #[arg(long)]
    pub json: bool,
    /// Named queue on the selected server client
    #[arg(long)]
    pub queue: Option<String>,
}

#[derive(Args)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    pub shell: clap_complete::Shell,
}

#[derive(Args)]
pub struct FeedbackArgs {
    /// Ledger ops; omitted = send a report
    #[command(subcommand)]
    pub action: Option<FeedbackAction>,
    /// One-line report title
    pub title: Option<String>,
    /// Report kind, rendered as the `[feedback/<kind>]` title prefix
    #[arg(long, default_value = "bug", value_parser = ["bug", "ux", "feature", "docs"])]
    pub kind: String,
    /// Report body inline (else --body-file, else stdin)
    #[arg(long, conflicts_with = "body_file")]
    pub body: Option<String>,
    /// Read the report body from a file
    #[arg(long)]
    pub body_file: Option<String>,
    /// Attach a client row's probe facts (never ssh.*, no network)
    #[arg(short, long)]
    pub client: Option<String>,
    /// Print the composed issue body and file nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Emit JSON (also the ls/retry forms)
    // global: `feedback ls --json` must parse — the flag lives on
    // the parent struct while `ls`/`retry` are subcommands.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum FeedbackAction {
    /// List local feedback artifacts
    Ls,
    /// Re-deliver pending artifacts (unique filename-stem prefix;
    /// omitted = all pending)
    Retry { id: Option<String> },
}

#[derive(Args)]
pub struct ProbeArgs {
    /// Deploy root to measure free disk on (default: /)
    #[arg(long, default_value = "/")]
    pub deploy_root: String,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long)]
    pub json: bool,
}
