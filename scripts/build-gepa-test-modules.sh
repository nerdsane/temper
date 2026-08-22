#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
shared_target="$repo_root/target/gepa-wasm"

modules=(
  gepa-replay
  gepa-reflective
  gepa-score
  gepa-pareto
  gepa-verify
)

for module in "${modules[@]}"; do
  manifest="$repo_root/wasm-modules/$module/Cargo.toml"
  cargo build \
    --manifest-path "$manifest" \
    --locked \
    --target wasm32-unknown-unknown \
    --target-dir "$shared_target" \
    --release

  artifact="${module//-/_}_module.wasm"
  fixture_dir="$repo_root/wasm-modules/$module/target/wasm32-unknown-unknown/release"
  mkdir -p "$fixture_dir"
  cp "$shared_target/wasm32-unknown-unknown/release/$artifact" "$fixture_dir/$artifact"
done
