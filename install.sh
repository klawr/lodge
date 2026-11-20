#!/usr/bin/env sh
# lodge installer — downloads a prebuilt release binary from GitHub.
#
#   # latest release:
#   curl -fsSL https://raw.githubusercontent.com/klawr/lodge/master/install.sh | sh
#
#   # a specific version / custom install location:
#   LODGE_VERSION=v0.1.0 LODGE_BIN_DIR=/usr/local/bin ./install.sh
set -eu

REPO="${LODGE_REPO:-klawr/lodge}"

VERSION="${LODGE_VERSION:-latest}"
BIN_DIR="${LODGE_BIN_DIR:-$HOME/.local/bin}"

err() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
info() { printf '\033[36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }

os="$(uname -s)"
arch="$(uname -m)"
[ "$os" = "Linux" ] || err "lodge currently ships a Linux binary only (got $os)"
case "$arch" in
  x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
  aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
  *) err "no prebuilt binary for $arch — build from source: cargo install --git https://github.com/$REPO" ;;
esac

asset="ldg-$target.tar.gz"
if [ "$VERSION" = "latest" ]; then
  url="https://github.com/$REPO/releases/latest/download/$asset"
else
  url="https://github.com/$REPO/releases/download/$VERSION/$asset"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  err "need curl or wget to download lodge"
fi

mkdir -p "$BIN_DIR"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
info "downloading $asset ($VERSION)"
fetch "$url" "$tmp/ldg.tar.gz" || err "download failed: $url"
tar -xzf "$tmp/ldg.tar.gz" -C "$tmp" || err "could not extract $asset"
install -m755 "$tmp/ldg" "$BIN_DIR/ldg" 2>/dev/null || { mv "$tmp/ldg" "$BIN_DIR/ldg"; chmod +x "$BIN_DIR/ldg"; }
info "installed ldg to $BIN_DIR/ldg"

# Warn about runtime dependencies lodge shells out to.
missing=""
for tool in docker tmux patchelf ldd; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
[ -n "$missing" ] && warn "missing required tools:$missing (install them via your package manager)"
command -v oras >/dev/null 2>&1 || info "note: 'oras' is only needed if your devcontainers use features (https://oras.land)"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH — add it, e.g.  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

info "done — run 'ldg up' in a project with a .devcontainer"
