#!/bin/sh
# Test scripts/install.sh against a local copy of a release, in a throwaway HOME:
#
#   scripts/test-install.sh <bedrock-mcp binary> [files to ship next to it...]
#
# Checks a fresh install, an update in place, a pinned tag, a missing release and a bad checksum.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
binary=$1
shift
work=$(mktemp -d)
server=""
trap 'test -n "$server" && kill "$server" 2>/dev/null; rm -rf "$work"' EXIT

fail() {
	echo "FAIL: $*" >&2
	exit 1
}

# A release folder laid out like the workflow's dist/.
asset=bedrock-mcp-linux-x86_64
mkdir -p "$work/dist/$asset"
cp "$binary" "$work/dist/$asset/bedrock-mcp"
for file in "$@"; do cp "$file" "$work/dist/$asset/"; done
cp "$root/README.md" "$root/LICENSE" "$work/dist/$asset/"
tar -C "$work/dist" -czf "$work/dist/$asset.tar.gz" "$asset"
rm -r "$work/dist/${asset:?}"
cp "$root/scripts/install.sh" "$work/dist/"
(cd "$work/dist" && sha256sum -- * >SHA256SUMS)

port=$((20000 + $$ % 20000))
python3 "$root/scripts/release-server.py" "$work/dist" v9.9.9 "$port" &
server=$!
export BEDROCK_MCP_RELEASES="http://127.0.0.1:$port/releases"
for _ in $(seq 50); do
	curl -fsI "$BEDROCK_MCP_RELEASES/latest" >/dev/null 2>&1 && break
	sleep 0.1
done

export HOME="$work/home" XDG_DATA_HOME="$work/home/.local/share" PATH="/usr/bin:/bin"
mkdir -p "$HOME"
install_dir=$XDG_DATA_HOME/bedrock-mcp
version=$("$binary" --version)

echo "1. fresh install with the documented one-liner (latest/download/install.sh piped to sh)"
out=$(curl -fsSL "$BEDROCK_MCP_RELEASES/latest/download/install.sh" | sh 2>&1) || fail "install failed: $out"
echo "$out" | grep -q "Installed ${version#bedrock-mcp }" || fail "no install message: $out"
echo "$out" | grep -q "\"command\": \"$install_dir/bedrock-mcp\"" || fail "no client config: $out"
echo "$out" | grep -q "Add $HOME/.local/bin to PATH" || fail "no PATH hint: $out"
[ "$("$HOME/.local/bin/bedrock-mcp" --version)" = "$version" ] || fail "the installed link does not run"
for file in "$@"; do
	[ -f "$install_dir/$(basename "$file")" ] || fail "$(basename "$file") was not installed"
done
[ -z "$(find "$install_dir" -name '.update.*')" ] || fail "a temporary folder was left behind"

echo "2. running it again updates in place"
out=$(sh "$root/scripts/install.sh" 2>&1) || fail "update failed: $out"
echo "$out" | grep -q "Updated ${version#bedrock-mcp } (was ${version#bedrock-mcp })" || fail "no update message: $out"

echo "3. a pinned tag, with and without the v"
sh "$root/scripts/install.sh" v9.9.9 >/dev/null 2>&1 || fail "install v9.9.9 failed"
sh "$root/scripts/install.sh" 9.9.9 >/dev/null 2>&1 || fail "install 9.9.9 failed"

echo "4. a release that does not exist"
out=$(sh "$root/scripts/install.sh" v0.0.1 2>&1) && fail "installing a missing release succeeded"
echo "$out" | grep -q "cannot download $asset.tar.gz from release v0.0.1" || fail "unclear error: $out"

echo "5. a bad checksum changes nothing"
before=$(sha256sum "$install_dir/bedrock-mcp")
sed -i "s/^[0-9a-f]\{64\}\(  $asset.tar.gz\)/$(printf '0%.0s' $(seq 64))\1/" "$work/dist/SHA256SUMS"
out=$(sh "$root/scripts/install.sh" 2>&1) && fail "a bad checksum was accepted"
echo "$out" | grep -q "does not match its checksum; nothing was changed" || fail "unclear error: $out"
[ "$(sha256sum "$install_dir/bedrock-mcp")" = "$before" ] || fail "a bad checksum changed the install"

echo "install.sh: all checks passed"
