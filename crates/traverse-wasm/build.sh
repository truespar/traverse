#!/usr/bin/env bash
# Build the @truespar/traverse-wasm npm package.
#
# Produces a publish-ready directory at js/ with this layout:
#   js/
#     package.json
#     index.js
#     index.d.ts
#     worker.js
#     pkg/                  # copied from wasm-pack output
#       traverse_wasm.js
#       traverse_wasm_bg.wasm
#       traverse_wasm.d.ts
#       ...
#
# Two variants supported:
#   ./build.sh             # single-threaded WASM (works on stable Rust)
#   ./build.sh --threads   # multi-threaded WASM (needs nightly + -Z build-std)
#
# Run from the workspace root or from crates/traverse-wasm/ - paths are
# normalized either way.

set -euo pipefail

# Resolve script directory regardless of where the user invoked from.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

THREADS=0
if [[ "${1:-}" == "--threads" ]]; then
  THREADS=1
fi

echo "-> Cleaning previous build output..."
rm -rf pkg/ js/pkg/


# Path remapping: with -Z build-std the panic-location strings in the
# binary point into the rustup toolchain source and the cargo registry,
# i.e. into this machine's home directory. Those prefixes (and the
# checkout) are mapped to neutral names. CARGO_ENCODED_RUSTFLAGS is used
# because the paths may contain spaces, and because it outranks every
# other rustflags source, so the thread flags below travel with it.
if command -v cygpath >/dev/null 2>&1; then
  _w() { cygpath -w "$1"; }
else
  _w() { printf '%s' "$1"; }
fi
_sep=$'\x1f'
_here="$(cd "$(dirname "$0")/../.." && pwd)"
_remap="--remap-path-prefix${_sep}$(_w "$_here")=src${_sep}"
_remap+="--remap-path-prefix${_sep}${_here}=src${_sep}"
_remap+="--remap-path-prefix${_sep}$(_w "${CARGO_HOME:-$HOME/.cargo}/registry/src")=dep:${_sep}"
_remap+="--remap-path-prefix${_sep}$(_w "${RUSTUP_HOME:-$HOME/.rustup}/toolchains")=toolchain:"
# Turn a space-separated flag list into the 0x1f-separated encoding.
_enc() { printf '%s' "$*" | tr ' ' "$_sep"; }

if [[ "$THREADS" == "1" ]]; then
  echo "-> Building multi-threaded WASM (nightly + build-std)..."
  # The flags must come from the environment, not .cargo/config.toml: the env
  # var beats every other source, so neither the workspace config nor
  # wasm-pack's defaults can shadow it. Getting them wrong still "succeeds" at
  # build time and fails at runtime when the thread pool starts ("#<Memory>
  # could not be cloned"):
  #   1. without +atomics the memory isn't shared;
  #   2. recent nightlies no longer derive --shared-memory from +atomics, so
  #      the wasm-ld flags are explicit, or the module keeps a private memory.
  # `-Z build-std` still comes from .cargo/config.toml's [unstable] table
  # (wasm-pack 0.14 rejects it as a CLI passthrough); it gives std itself the
  # atomics support --shared-memory needs. wasm-bindgen's threads transform
  # (0.2.126) needs the memory IMPORTED (it panics on a defined memory) and the
  # TLS bootstrap symbols exported to set up thread-locals in each worker; older
  # rustc emitted all this for +atomics, current nightly doesn't.
  #   3. __heap_base must be exported too: the transform injects the thread id
  #      there and otherwise fails with "failed to find `__heap_base` for
  #      injecting thread id".
  CARGO_ENCODED_RUSTFLAGS="$(_enc -C target-feature=+atomics,+bulk-memory,+mutable-globals \
    -C link-arg=--shared-memory \
    -C link-arg=--max-memory=1073741824 \
    -C link-arg=--import-memory \
    -C link-arg=--export=__wasm_init_tls \
    -C link-arg=--export=__tls_size \
    -C link-arg=--export=__tls_align \
    -C link-arg=--export=__tls_base \
    -C link-arg=--export=__heap_base)${_sep}${_remap}" \
    wasm-pack build --target web --release --features threads
else
  echo "-> Building single-threaded WASM..."
  # Do not inherit the crate's threaded target flags or nightly override.
  RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}" \
    CARGO_ENCODED_RUSTFLAGS="$(_enc -C target-feature=+bulk-memory,+mutable-globals)${_sep}${_remap}" \
    wasm-pack build --target web --release
fi

echo "-> Copying pkg/ into js/pkg/ for the npm package layout..."
mkdir -p js/pkg
cp -r pkg/* js/pkg/
# The npm package's real package.json lives at js/; the wasm-pack one
# in js/pkg/ stays because the threaded build's wasm-bindgen-rayon
# snippet resolves the main module with a directory import
# (`import('../../..')`) that vite 8 / rolldown can only resolve
# through js/pkg/package.json's `main` field. Nested package.json
# files are inert for npm publishing, so keeping it is harmless.
rm -f js/pkg/.gitignore js/pkg/README.md

echo "-> Done. Publish-ready package at:"
echo "    $SCRIPT_DIR/js/"
echo
echo "  Test locally:"
echo "    cd js/ && npm pack    # -> @truespar-traverse-wasm-X.Y.Z.tgz"
echo
echo "  Publish to npm:"
echo "    cd js/ && npm publish"
