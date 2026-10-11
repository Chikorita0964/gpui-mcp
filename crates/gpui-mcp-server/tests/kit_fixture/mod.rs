//! The GPUI Kit example built on GPUI Fast, the fixture of the tests that need
//! the backend this fork uses.
//!
//! The example is a workspace of its own (Cargo features are additive, and the
//! Zed demos must not turn on their backend in the bridge it uses), so no
//! `CARGO_BIN_EXE` names it: it is built once per test run, with the cargo that
//! runs the tests, into `target/kit-fast`. An up-to-date build is a no-op. A
//! build through a compiler wrapper gets a directory of its own
//! (`target/kit-fast-<wrapper>`): kache restores outputs as read-only hardlinks,
//! which a build without it cannot overwrite.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The fixture's binary, built once per test run.
pub(crate) fn executable() -> Result<PathBuf, String> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT.get_or_init(build).clone()
}

fn build() -> Result<PathBuf, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let wrapper = std::env::var_os("RUSTC_WRAPPER").filter(|wrapper| !wrapper.is_empty()).and_then(
        |wrapper| Path::new(&wrapper).file_stem().map(|stem| stem.to_string_lossy().into_owned()),
    );
    let target = root.join(match wrapper {
        Some(wrapper) => format!("target/kit-fast-{wrapper}"),
        None => "target/kit-fast".to_owned(),
    });
    let status = std::process::Command::new(env!("CARGO"))
        .arg("build")
        .arg("--manifest-path")
        .arg(root.join("examples/gpui-kit/Cargo.toml"))
        .args(["--no-default-features", "--features", "gpui-fast", "--locked", "--quiet"])
        .arg("--target-dir")
        .arg(&target)
        .status()
        .map_err(|error| format!("could not run cargo to build the fixture: {error}"))?;
    if !status.success() {
        return Err(format!("building the GPUI Kit fixture on GPUI Fast failed: {status}"));
    }
    let path =
        target.join("debug").join(format!("gpui-mcp-kit-demo{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("the GPUI Kit fixture is not at {} after its build", path.display()))
    }
}
