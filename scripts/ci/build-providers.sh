#!/usr/bin/env bash
set -euo pipefail

# Each artifact is read from where this build writes it.
target_dir="$(bash "$(dirname "${BASH_SOURCE[0]}")/target-dir.sh")"
wasm_dir="$target_dir/wasm32-wasip2/release"

cargo build --target wasm32-wasip2 --release \
  -p 'omnifs-provider-*' \
  -p test-provider

shopt -s nullglob
wasms=("$wasm_dir"/omnifs_provider_*.wasm "$wasm_dir"/test_provider.wasm)
if (( ${#wasms[@]} == 0 )); then
  printf 'no provider WASM components found\n' >&2
  exit 1
fi

bundle_wasms=("$wasm_dir"/omnifs_provider_*.wasm)
cargo build --release -p omnifs-provider --bin omnifs-provider-store-bundle
"$target_dir/release/omnifs-provider-store-bundle" \
  --out "$target_dir/omnifs-provider-store" \
  "${bundle_wasms[@]}"
