#!/usr/bin/env sh
set -e

REPO="nkitsaini/airman"
BINARY="airman"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# Detect OS
OS="$(uname -s)"
if [ "$OS" != "Linux" ]; then
  echo "Error: airman currently supports Linux only (uses native Linux D-Bus APIs)." >&2
  exit 1
fi

# Detect Architecture
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) TARGET="x86_64-unknown-linux-musl" ;;
  aarch64|arm64) TARGET="aarch64-unknown-linux-musl" ;;
  *)
    echo "Error: Unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

# Get latest release tag
LATEST_TAG="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')"
if [ -z "$LATEST_TAG" ]; then
  echo "Error: Could not determine latest release tag." >&2
  exit 1
fi

URL="https://github.com/${REPO}/releases/download/${LATEST_TAG}/airman-${LATEST_TAG}-${TARGET}.tar.gz"

echo "Downloading airman ${LATEST_TAG} for ${TARGET}..."
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL "$URL" | tar -xz -C "$TMPDIR"

mkdir -p "$INSTALL_DIR"
cp "$TMPDIR/airman-${LATEST_TAG}-${TARGET}/airman" "$INSTALL_DIR/$BINARY"
chmod +x "$INSTALL_DIR/$BINARY"

echo "Successfully installed airman to $INSTALL_DIR/$BINARY"
echo "Make sure $INSTALL_DIR is in your PATH."
