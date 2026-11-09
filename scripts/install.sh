#!/bin/sh
# Install or update Bedrock MCP for Minecraft by Red Eagle Studios on Linux x86_64:
#
#   curl -fsSL https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/RedEagleStudios/bedrock-mcp/releases/latest/download/install.sh | sh -s -- v0.2.0
#
# Running it again updates in place. Each file is replaced by a rename, so a server that is running
# keeps its files until the MCP client restarts it. BEDROCK_MCP_RELEASES points it at another
# release location with the same layout (a mirror, or a local copy for testing).
set -eu

releases=${BEDROCK_MCP_RELEASES:-https://github.com/RedEagleStudios/bedrock-mcp/releases}
asset=bedrock-mcp-linux-x86_64

fail() {
	echo "bedrock-mcp install: $*" >&2
	exit 1
}

# Everything runs from here, so a download cut short runs nothing.
main() {
	if [ "${OS:-}" = Windows_NT ]; then
		fail "on Windows, run this in PowerShell instead: irm $releases/latest/download/install.ps1 | iex"
	fi
	[ "$(uname -sm)" = "Linux x86_64" ] ||
		fail "there is no build for $(uname -sm). Releases cover Linux x86_64 and Windows x86_64."
	for tool in curl tar sha256sum; do
		command -v "$tool" >/dev/null || fail "$tool is required."
	done

	if [ $# -gt 0 ]; then
		case $1 in
		v*) tag=$1 ;;
		*) tag=v$1 ;;
		esac
	else
		# releases/latest redirects to releases/tag/<newest tag>.
		tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$releases/latest") ||
			fail "cannot reach $releases."
		tag=${tag##*/}
		case $tag in
		v[0-9]*) ;;
		*) fail "$releases has no release yet." ;;
		esac
	fi

	data=${XDG_DATA_HOME:-$HOME/.local/share}
	dir=$data/bedrock-mcp
	bin_dir=$HOME/.local/bin
	mkdir -p "$dir" "$bin_dir"
	# Inside the install folder, so moving the new files in is a rename on one filesystem.
	tmp=$(mktemp -d "$dir/.update.XXXXXX")
	trap 'rm -rf "$tmp"' EXIT
	trap 'exit 1' INT TERM

	echo "Downloading bedrock-mcp $tag"
	curl -fL --progress-bar -o "$tmp/$asset.tar.gz" "$releases/download/$tag/$asset.tar.gz" ||
		fail "cannot download $asset.tar.gz from release $tag."
	curl -fsSL -o "$tmp/SHA256SUMS" "$releases/download/$tag/SHA256SUMS" ||
		fail "cannot download SHA256SUMS from release $tag."
	(cd "$tmp" && grep -E "^[0-9a-f]{64} [ *]$asset\.tar\.gz\$" SHA256SUMS >"$asset.sha256") ||
		fail "SHA256SUMS of release $tag does not list $asset.tar.gz."
	(cd "$tmp" && sha256sum -c --quiet "$asset.sha256") ||
		fail "$asset.tar.gz does not match its checksum; nothing was changed."
	tar -xzf "$tmp/$asset.tar.gz" -C "$tmp"
	[ -x "$tmp/$asset/bedrock-mcp" ] || fail "$asset.tar.gz holds no bedrock-mcp."
	installed=$("$tmp/$asset/bedrock-mcp" --version </dev/null) ||
		fail "the downloaded bedrock-mcp does not run on this system; nothing was changed."

	previous=""
	if [ -x "$dir/bedrock-mcp" ]; then
		previous=$("$dir/bedrock-mcp" --version </dev/null 2>/dev/null || true)
		case $previous in
		"bedrock-mcp "*) previous=${previous#bedrock-mcp } ;;
		*) previous="an unknown version" ;;
		esac
	fi
	for file in "$tmp/$asset"/*; do
		mv -f "$file" "$dir/"
	done
	ln -sf "$dir/bedrock-mcp" "$bin_dir/bedrock-mcp"

	echo
	if [ -n "$previous" ]; then
		echo "Updated ${installed#bedrock-mcp } (was $previous) in $dir."
		echo "Restart your MCP client to load it."
	else
		echo "Installed ${installed#bedrock-mcp } in $dir."
		echo
		echo "MCP client configuration:"
		echo
		echo "  { \"mcpServers\": { \"minecraft\": { \"command\": \"$dir/bedrock-mcp\" } } }"
	fi
	case ":$PATH:" in
	*":$bin_dir:"*) ;;
	*) echo "Add $bin_dir to PATH to run bedrock-mcp from a shell." ;;
	esac
	if [ ! -d "${MCBE_GDK_ROOT:-$data/mcbe-gdk-linux}" ]; then
		echo "mcbe-gdk-linux is not installed. bedrock-mcp attaches to a game any launcher started,"
		echo "and starts one itself with mcbe-gdk-linux: https://gitlab.com/veedy-dev/mcbe-gdk-installer"
	fi
	echo "Run this installer again to update."
}

main "$@"
