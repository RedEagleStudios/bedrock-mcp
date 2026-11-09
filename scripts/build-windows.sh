#!/usr/bin/env bash
# Build the Windows crates (bedrock_agent.dll, bedrock_unstable.dll, bedrock-inject.exe) in release
# mode for bedrock-mcp on Linux, which injects them into the game under Wine. On Windows use
# scripts/build-windows.ps1 instead.
#
# Uses the host toolchain when it has the x86_64-pc-windows-gnu target and a
# MinGW linker; otherwise builds in Docker (scripts/windows-build.Dockerfile).
# Artifacts land in target/x86_64-pc-windows-gnu/release, where bedrock-mcp
# looks for them.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=x86_64-pc-windows-gnu
CRATES=(-p bedrock-agent -p bedrock-unstable -p bedrock-inject)

host_can_build() {
  command -v x86_64-w64-mingw32-gcc >/dev/null &&
    rustc --print target-libdir --target "$TARGET" 2>/dev/null | xargs test -d
}

if host_can_build; then
  cargo build --release --target "$TARGET" "${CRATES[@]}" --manifest-path "$ROOT/Cargo.toml"
  exit 0
fi

command -v docker >/dev/null || {
  echo "Install the $TARGET Rust target and x86_64-w64-mingw32-gcc, or Docker." >&2
  exit 1
}
IMAGE=bedrock-mcp-windows-build
docker build -q -t "$IMAGE" -f "$ROOT/scripts/windows-build.Dockerfile" "$ROOT/scripts" >/dev/null
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/bedrock-mcp/cargo"
mkdir -p "$CACHE"
# A separate target directory keeps the container's host build scripts from
# invalidating the host's own builds.
docker run --rm -u "$(id -u):$(id -g)" \
  -v "$ROOT:/work" -v "$CACHE:/cargo" -e CARGO_HOME=/cargo -w /work "$IMAGE" \
  cargo build --release --target "$TARGET" "${CRATES[@]}" --target-dir target/windows
mkdir -p "$ROOT/target/$TARGET/release"
cp "$ROOT/target/windows/$TARGET/release/bedrock_agent.dll" \
  "$ROOT/target/windows/$TARGET/release/bedrock_unstable.dll" \
  "$ROOT/target/windows/$TARGET/release/bedrock-inject.exe" \
  "$ROOT/target/$TARGET/release/"
