#!/bin/sh
# Install the cua-rs prebuilt binary from GitHub Releases.
#   curl -fsSL https://raw.githubusercontent.com/maestrojeong/cua-rs-mcp/main/install.sh | sh
# Env: CUA_VERSION (default: latest), CUA_BIN_DIR (default: /usr/local/bin or ~/.local/bin)
set -e

REPO="maestrojeong/cua-rs-mcp"
VERSION="${CUA_VERSION:-latest}"

OS="$(uname -s)"
ARCH="$(uname -m)"
# macOS only, by construction: this server is a wrapper around the macOS
# Accessibility API and ScreenCaptureKit. There is nothing to port.
case "$OS-$ARCH" in
  Darwin-arm64)  ASSET="cua-rs-macos-arm64" ;;
  Darwin-x86_64) echo "No prebuilt Intel-mac yet."; NEED_SRC=1 ;;
  *)             echo "cua-rs is macOS-only (got $OS-$ARCH)."; exit 1 ;;
esac
if [ "${NEED_SRC:-0}" = "1" ]; then
  echo "Build from source instead:"
  echo "  cargo install --git https://github.com/$REPO cua-mcp"
  exit 1
fi

if [ "$VERSION" = "latest" ]; then
  URL="https://github.com/$REPO/releases/latest/download/$ASSET"
else
  URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"
fi

if [ -n "${CUA_BIN_DIR:-}" ]; then
  DEST="$CUA_BIN_DIR"
  mkdir -p "$DEST"
else
  DEST="/usr/local/bin"
  if ! ( [ -d "$DEST" ] && [ -w "$DEST" ] ); then
    DEST="$HOME/.local/bin"
    mkdir -p "$DEST"
  fi
fi

echo "Downloading $ASSET ($VERSION) -> $DEST/cua-rs"
curl -fsSL "$URL" -o "$DEST/cua-rs"
chmod +x "$DEST/cua-rs"

# curl does not set com.apple.quarantine, so this is belt-and-braces -- but the
# failure it prevents is bad enough to be worth two lines. A quarantined ad-hoc
# binary does not error on launch, it *blocks*, and an MCP client that spawned it
# then waits forever for a handshake that will never arrive.
xattr -d com.apple.quarantine "$DEST/cua-rs" 2>/dev/null || true

echo "Installed: $DEST/cua-rs"

# TCC grants attach to the launching process, so the install path alone is not
# enough -- tell the user what actually has to be approved.
echo
echo "Next: grant Accessibility (required) and Screen Recording (for screenshots)"
echo "to the app that will LAUNCH cua-rs -- your terminal or MCP client, not the"
echo "cua-rs binary. System Settings > Privacy & Security."
echo "Check with:  $DEST/cua-rs permissions"

case ":$PATH:" in
  *":$DEST:"*) echo; echo "Run: cua-rs --help" ;;
  *) echo; echo "Add to PATH:  export PATH=\"$DEST:\$PATH\"   then: cua-rs --help" ;;
esac
