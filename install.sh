#!/usr/bin/env sh
# corc installer
#
# Usage:
#   curl -fsSL https://github.com/HectorBjernersjo/corc/releases/latest/download/install.sh | sh
#
# Env vars:
#   CORC_VERSION     Pin a specific tag (e.g. v0.2.0). Defaults to "latest".
#   CORC_INSTALL_DIR Where to drop the binary. Defaults to $HOME/.local/bin.

set -eu

REPO="HectorBjernersjo/corc"
VERSION="${CORC_VERSION:-latest}"
INSTALL_DIR="${CORC_INSTALL_DIR:-$HOME/.local/bin}"

uname_s="$(uname -s)"
uname_m="$(uname -m)"

case "$uname_s" in
    Linux)  os="unknown-linux-musl" ;;
    Darwin) os="apple-darwin" ;;
    *) echo "unsupported OS: $uname_s (corc requires tmux and is Unix-only)" >&2; exit 1 ;;
esac

case "$uname_m" in
    x86_64|amd64) arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *) echo "unsupported arch: $uname_m" >&2; exit 1 ;;
esac

target="${arch}-${os}"
asset="corc-${target}.tar.gz"

if [ "$VERSION" = "latest" ]; then
    url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
    url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
fi

echo "Installing corc (${VERSION}) for ${target} to ${INSTALL_DIR}"

mkdir -p "$INSTALL_DIR"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "$url" -o "$tmp/corc.tar.gz"
tar -xzf "$tmp/corc.tar.gz" -C "$tmp"
mv "$tmp/corc" "$INSTALL_DIR/corc"
chmod +x "$INSTALL_DIR/corc"

echo "Installed: $INSTALL_DIR/corc"

echo ""
echo "Get started:"
echo ""
echo "  corc         # launch the TUI picker"
echo "  corc open    # open/switch to the corc session"
echo "  corc list    # list conversations corc owns"

if ! command -v tmux >/dev/null 2>&1; then
    echo ""
    echo "NOTE: corc requires tmux, which was not found on your PATH. Install it first."
fi

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo ""; echo "NOTE: $INSTALL_DIR is not in your PATH. Add it to your shell rc:"; echo "  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
