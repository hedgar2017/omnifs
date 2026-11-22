#!/usr/bin/env bash
# Print the directory that cargo builds into, as cargo itself resolves it from
# CARGO_TARGET_DIR, CARGO_BUILD_TARGET_DIR, and build.target-dir of its
# configuration. Every script and recipe that reads a build artifact asks
# here: this is the one shell owner of where cargo builds.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
metadata="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml")"
target_dir="$(sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' <<<"$metadata")"
if [[ -z "$target_dir" || "$target_dir" == *\\* ]]; then
  printf 'cargo metadata names no target directory that this script reads: %s\n' "$target_dir" >&2
  exit 1
fi
printf '%s\n' "$target_dir"
