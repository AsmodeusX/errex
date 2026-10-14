#!/usr/bin/env bash
# Build a statically linked errexd for wp-vps (Ubuntu 24.04, x86_64).
#
#   deploy/wp-vps/build-release.sh [OUT_DIR]
#
# Builds the SPA first (it is embedded in the binary at compile time), then
# errexd with the repo's release profile (opt-level z, fat LTO, stripped,
# panic=abort). Prefers the musl target, which needs no runtime libraries:
#
#   rustup target add x86_64-unknown-linux-musl
#   sudo apt install musl-tools
#
# Without those it falls back to a static glibc build. That binary runs on
# wp-vps only while the build host's glibc matches the server's (2.39 on
# Ubuntu 24.04): DNS lookups load the host's NSS modules at run time.
#
# Honours CARGO_TARGET_DIR and TMPDIR. Writes errexd and errexd.sha256 to
# OUT_DIR (default: ./errex-release).
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
out=${1:-$PWD/errex-release}
target_dir=${CARGO_TARGET_DIR:-$repo/target}

# ----- SPA -----
cd "$repo/web"
if command -v bun >/dev/null 2>&1; then
    bun install --frozen-lockfile
    bun run build
elif [ -d node_modules ]; then
    npx svelte-kit sync
    npx vite build
else
    echo "build-release: install bun (https://bun.sh) or populate web/node_modules" >&2
    exit 1
fi
[ -f build/index.html ] || { echo "build-release: SPA build produced no index.html" >&2; exit 1; }

# ----- daemon -----
cd "$repo"
if rustup target list --installed 2>/dev/null | grep -qx x86_64-unknown-linux-musl \
    && command -v musl-gcc >/dev/null 2>&1; then
    target=x86_64-unknown-linux-musl
    CC_x86_64_unknown_linux_musl=musl-gcc \
        cargo build --release --locked -p errexd --target "$target"
else
    target=x86_64-unknown-linux-gnu
    echo "build-release: musl target or musl-gcc missing; building static glibc (needs glibc $(ldd --version | head -1 | awk '{print $NF}') on the server)" >&2
    RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static" \
        cargo build --release --locked -p errexd --target "$target"
fi

bin="$target_dir/$target/release/errexd"
if ! file "$bin" | grep -Eq 'statically linked|static-pie linked'; then
    echo "build-release: $bin is not statically linked:" >&2
    file "$bin" >&2
    exit 1
fi

mkdir -p "$out"
install -m 0755 "$bin" "$out/errexd"
(cd "$out" && sha256sum errexd > errexd.sha256)
echo "build-release: $out/errexd ($target)"
cat "$out/errexd.sha256"
