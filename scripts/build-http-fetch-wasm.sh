#!/usr/bin/env bash
# Rebuild the built-in http_fetch WASM module from wasm-modules/http-fetch and copy it to
# crates/temper-wasm/modules/http_fetch.wasm, the binary temper-server embeds.
#
# The toolchain is the one rust-toolchain.toml pins, and the build is --locked. Cargo derives each
# package's build hash, which names its symbols and orders its functions in the output, from the
# package's path: relative to the workspace root for a package inside it, absolute otherwise. The
# SDK sits outside the module's own workspace, so the build copies both into one staging workspace,
# where both paths are relative. Source paths in panic strings are remapped, and crate metadata
# is fixed per package (see below). A build therefore matches wherever the checkout sits and
# whatever the host, and CI runs --check.
#
#   scripts/build-http-fetch-wasm.sh          rebuild and replace the embedded binary
#   scripts/build-http-fetch-wasm.sh --check  rebuild and compare with the embedded binary
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
module_dir="$root/wasm-modules/http-fetch"
embedded="$root/crates/temper-wasm/modules/http_fetch.wasm"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
stage="$(cd "$stage" && pwd -P)"
mkdir -p "$stage/crates" "$stage/wasm-modules/http-fetch"
cp "$root/rust-toolchain.toml" "$stage/"
cp -R "$root/crates/temper-wasm-sdk" "$stage/crates/"
cp -R "$module_dir/src" "$stage/wasm-modules/http-fetch/"
cp "$module_dir/Cargo.lock" "$stage/"
# The staging root is the module's workspace here, so its own [workspace] table is dropped.
grep -vx '\[workspace\]' "$module_dir/Cargo.toml" > "$stage/wasm-modules/http-fetch/Cargo.toml"
printf '[workspace]\nmembers = ["wasm-modules/http-fetch"]\nresolver = "3"\n' > "$stage/Cargo.toml"

# Standard library code inlined into the module carries its source path. rustc names it
# /rustc/<commit>/library, unless the toolchain has the rust-src component, when it names the
# local copy under the toolchain's own directory instead. -Z keeps the /rustc name either way,
# so the build does not depend on which components a machine installed.
export RUSTFLAGS="--remap-path-prefix=$stage=/temper --remap-path-prefix=$cargo_home=/cargo -Ztranslate-remapped-path-to-local-path=no"
export CARGO_TARGET_DIR="$stage/target"

# Cargo derives each crate's -C metadata from its dependencies' metadata, and a build script or
# proc macro is built for the host, so the host's triple reaches the metadata of every crate that
# uses one. That metadata names the crate in each symbol and orders its functions, so macOS and
# Linux built different binaries from the same source. The wrapper gives each crate built for the
# wasm target the metadata of its package name and version instead, which no host changes.
cat > "$stage/rustc-wrapper" <<'WRAPPER'
#!/usr/bin/env bash
rustc="$1"
shift
for arg in "$@"; do
  if [[ "$arg" == "--target" ]]; then
    args=()
    for arg in "$@"; do
      case "$arg" in
        metadata=*) args+=("metadata=$CARGO_PKG_NAME-$CARGO_PKG_VERSION") ;;
        *) args+=("$arg") ;;
      esac
    done
    exec "$rustc" "${args[@]}"
  fi
done
exec "$rustc" "$@"
WRAPPER
chmod +x "$stage/rustc-wrapper"
export RUSTC_WRAPPER="$stage/rustc-wrapper"

(cd "$stage" && cargo build --locked --release --target wasm32-unknown-unknown)
built="$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/http_fetch_module.wasm"

if [[ "${1:-}" == "--check" ]]; then
  if cmp -s "$built" "$embedded"; then
    echo "http_fetch.wasm matches its source"
  else
    echo "http_fetch.wasm differs from a build of wasm-modules/http-fetch" >&2
    shasum -a 256 "$built" "$embedded" >&2
    exit 1
  fi
else
  cp "$built" "$embedded"
  shasum -a 256 "$embedded"
fi
