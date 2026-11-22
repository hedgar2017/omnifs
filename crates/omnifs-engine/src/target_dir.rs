//! Where cargo builds: the one Rust owner of the target directory, for the
//! build script and the test harnesses that read a build artifact. This file
//! uses `std` and `serde_json` alone, so that a build script includes it by
//! its path.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The directory that cargo builds the workspace of `workspace` into, as
/// cargo itself resolves it from `CARGO_TARGET_DIR`,
/// `CARGO_BUILD_TARGET_DIR`, and `build.target-dir` of its configuration.
///
/// # Panics
///
/// Panics when cargo does not answer its metadata, which no caller can
/// continue without.
#[must_use]
pub fn cargo_target_dir(workspace: &Path) -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace)
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata is JSON");
    PathBuf::from(
        metadata["target_directory"]
            .as_str()
            .expect("cargo metadata names its target directory"),
    )
}
