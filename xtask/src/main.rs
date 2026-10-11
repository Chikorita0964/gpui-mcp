//! Maintenance tasks for gpui-mcp. Run `cargo xtask --help` from a checkout.

mod consumer;
mod patch;
mod vendor;

use std::path::Path;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Maintenance tasks for gpui-mcp")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Write a patched crates.io GPUI crate, or verify the vendored copy.
    ///
    /// Downloads the release from crates.io, checks its checksum, and applies
    /// the crate's patch series from `vendor/patches/<crate>/<version>/`.
    /// Without --output, writes `vendor/<crate>`. Nothing is replaced if a
    /// patch fails to apply.
    Vendor(vendor::VendorArgs),
    /// Build a fresh app that installs the bridge from a pushed Git commit.
    CheckConsumer(consumer::ConsumerArgs),
}

fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask lives inside the workspace")?;
    match Cli::parse().task {
        Task::Vendor(args) => vendor::run(root, &args),
        Task::CheckConsumer(args) => consumer::run(&args),
    }
}
