//! `cargo xtask check-consumer`: build a standalone app that installs the
//! bridge from a published Git commit.
//!
//! The app follows the README's recipe: GPUI Kit on GPUI Fast. It type-checks
//! `BridgeHandle::install` with GPUI Kit's `Window` and `App`, and verifies that
//! the app's GPUI crates and the bridge all share the one patched GPUI Fast
//! package from the requested commit.
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Args, ValueEnum};
use cow_utils::CowUtils as _;
use rustc_hash::FxHashMap;
use serde_json::Value;

#[derive(Clone, Copy, ValueEnum)]
pub enum Backend {
    /// GPUI Kit from its GitHub main with its `gpui-fast` feature, on GPUI Fast
    /// 0.1.6.
    #[value(name = "gpui-kit")]
    Kit,
}

struct Recipe {
    /// The app's own dependencies.
    dependencies: &'static str,
    /// The crates.io GPUI package the Git patch replaces, and its version.
    gpui: (&'static str, &'static str),
    /// The path the app reaches GPUI's types through.
    types: &'static str,
    /// Every package that must link the one patched GPUI.
    owners: &'static [&'static str],
    /// Whether the owners link no other GPUI. GPUI Kit's crates keep their
    /// gpui-pre dependency when its `gpui-fast` feature selects GPUI Fast.
    exclusive: bool,
}

impl Backend {
    fn name(self) -> &'static str {
        match self {
            Self::Kit => "gpui-kit",
        }
    }

    fn recipe(self) -> Recipe {
        match self {
            Self::Kit => Recipe {
                dependencies: "gpui-kit = { git = \"https://github.com/longbridge/gpui-kit\", \
                               rev = \"ed27c6293b585acc40c10be7138534243bee5841\", \
                               features = [\"gpui-fast\"] }",
                gpui: ("gpui-fast", "0.1.6"),
                types: "gpui_kit",
                owners: &[
                    "gpui-base",
                    "gpui-component",
                    "gpui-kit",
                    "gpui-mcp",
                    "gpui-fast-platform",
                ],
                exclusive: false,
            },
        }
    }
}

#[derive(Args)]
pub struct ConsumerArgs {
    /// Which README recipe to check.
    #[arg(long, value_enum, default_value = "gpui-kit")]
    backend: Backend,
    /// The gpui-mcp Git repository URL.
    #[arg(long)]
    repository: String,
    /// The full commit SHA to install from.
    #[arg(long)]
    rev: String,
    /// An optional shared Cargo build cache.
    #[arg(long)]
    target_dir: Option<PathBuf>,
}

#[expect(clippy::print_stdout, reason = "xtask is a command-line tool: its result goes to stdout")]
pub fn run(args: &ConsumerArgs) -> Result<()> {
    ensure!(
        args.rev.len() == 40 && args.rev.chars().all(|c| c.is_ascii_hexdigit()),
        "--rev must be a full 40-character commit SHA"
    );
    let recipe = args.backend.recipe();
    let (gpui_package, gpui_version) = recipe.gpui;
    let repository = serde_json::to_string(&args.repository)?;
    let rev = serde_json::to_string(&args.rev)?;
    let target_dir = args.target_dir.as_deref().map(std::path::absolute).transpose()?;

    let consumer =
        tempfile::Builder::new().prefix(&format!("{}-consumer-", args.backend.name())).tempdir()?;
    let root = consumer.path();
    fs::create_dir(root.join("src"))?;
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{name}-git-consumer"
version = "0.0.0"
edition = "2024"
rust-version = "1.99"

[workspace]

[dependencies]
{dependencies}
gpui-mcp = {{ git = {repository}, rev = {rev} }}

[patch.crates-io]
{gpui_package} = {{ git = {repository}, rev = {rev} }}
"#,
            name = args.backend.name(),
            dependencies = recipe.dependencies,
        ),
    )?;
    fs::write(
        root.join("src/main.rs"),
        format!(
            r#"use gpui_mcp::{{AppId, BridgeConfig, BridgeHandle}};

fn install(window: &mut {types}::Window, cx: &mut {types}::App)
    -> Result<BridgeHandle, Box<dyn std::error::Error>>
{{
    Ok(BridgeHandle::install(
        window, cx, BridgeConfig::new(AppId::new("git-consumer")?, "Git consumer"),
    )?)
}}

fn main() {{
    // Type-check real bridge installation with the backend's GPUI types.
    let _ = install;
}}
"#,
            types = recipe.types,
        ),
    )?;

    let cargo = |arguments: &[&str]| {
        let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        command.args(arguments).current_dir(root);
        if let Some(target_dir) = &target_dir {
            command.env("CARGO_TARGET_DIR", target_dir);
        }
        command
    };
    let status = cargo(&["check"]).status()?;
    ensure!(status.success(), "cargo check failed for the {} consumer", args.backend.name());
    let output = cargo(&["metadata", "--format-version=1", "--locked"]).output()?;
    ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    verify(&metadata, &recipe, &args.rev)?;
    println!(
        "{}: all {} GPUI consumers use patched {gpui_package} {gpui_version} at {}",
        args.backend.name(),
        recipe.owners.len(),
        args.rev
    );
    Ok(())
}

/// Check that exactly one patched GPUI package from `rev` was resolved, and
/// that every owner links it and no other GPUI.
fn verify(metadata: &Value, recipe: &Recipe, rev: &str) -> Result<()> {
    let (gpui_package, gpui_version) = recipe.gpui;
    let packages = metadata["packages"].as_array().context("metadata has no packages")?;
    let packages_named = |name: &str| -> Vec<&Value> {
        packages.iter().filter(|package| package["name"] == name).collect()
    };
    let [snapshot] = packages_named(gpui_package)[..] else {
        bail!("expected one {gpui_package} package; found {}", packages_named(gpui_package).len());
    };
    let source = snapshot["source"].as_str().unwrap_or_default();
    if snapshot["version"] != gpui_version
        || !source.ends_with(&format!("#{}", rev.cow_to_lowercase()))
    {
        bail!("{gpui_package} did not resolve to the requested patched Git snapshot");
    }
    let names: FxHashMap<&str, &str> = packages
        .iter()
        .filter_map(|package| Some((package["id"].as_str()?, package["name"].as_str()?)))
        .collect();
    let nodes: FxHashMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .context("metadata has no resolve graph")?
        .iter()
        .filter_map(|node| Some((node["id"].as_str()?, node)))
        .collect();
    for owner in recipe.owners {
        let [package] = packages_named(owner)[..] else {
            bail!("expected one {owner} package; found {}", packages_named(owner).len());
        };
        let id = package["id"].as_str().context("package has no id")?;
        let gpui: Vec<&str> = nodes[id]["deps"]
            .as_array()
            .context("node has no deps")?
            .iter()
            .filter_map(|dep| dep["pkg"].as_str())
            .filter(|pkg| matches!(names.get(pkg), Some(&("gpui" | "gpui-pre" | "gpui-fast"))))
            .collect();
        let patched = snapshot["id"].as_str().unwrap_or_default();
        let links_patched =
            if recipe.exclusive { gpui == [patched] } else { gpui.contains(&patched) };
        if !links_patched {
            bail!("{owner} does not use the patched {gpui_package} package: {gpui:?}");
        }
    }
    Ok(())
}
