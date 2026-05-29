#!/bin/sh
set -e

# flat-cyborg installer.
#
#   curl -fsSL https://raw.githubusercontent.com/Replikanti/flat-cyborg/main/install.sh | sh
#
# Downloads the latest release binary for this OS/architecture from the
# project's GitHub Releases, verifies its SHA256 checksum, and installs it.
# Override the destination with FLAT_CYBORG_INSTALL_DIR (default /usr/local/bin).

REPO="Replikanti/flat-cyborg"
INSTALL_DIR="${FLAT_CYBORG_INSTALL_DIR:-/usr/local/bin}"

# Detect OS and architecture.
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  PLATFORM="linux" ;;
  Darwin) PLATFORM="macos" ;;
  *)
    echo "Error: unsupported OS: $OS" >&2
    echo "Download manually from https://github.com/$REPO/releases" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64)  ARCH_NAME="x86_64" ;;
  aarch64|arm64) ARCH_NAME="aarch64" ;;
  *)
    echo "Error: unsupported architecture: $ARCH" >&2
    echo "Download manually from https://github.com/$REPO/releases" >&2
    exit 1
    ;;
esac

BINARY="flat-cyborg-${PLATFORM}-${ARCH_NAME}"

# Helper: download a URL to a file (or stdout if no output path given).
fetch() {
  url="$1"
  out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" ${out:+-o "$out"}
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "${out:--}" "$url"
  else
    echo "Error: curl or wget required" >&2
    exit 1
  fi
}

# Helper: print the SHA256 of a file, or empty string if no tool is available.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo ""
  fi
}

# Resolve the latest release tag.
RELEASE_JSON=$(fetch "https://api.github.com/repos/$REPO/releases/latest" "")
LATEST=$(echo "$RELEASE_JSON" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

if [ -z "$LATEST" ]; then
  echo "Error: could not determine latest release" >&2
  exit 1
fi

BASE_URL="https://github.com/$REPO/releases/download/${LATEST}"

echo "Installing flat-cyborg ${LATEST} (${PLATFORM}/${ARCH_NAME})..."

# Download the binary and its checksum.
TMPDIR_BASE="${TMPDIR:-/tmp}"
TMPFILE=$(mktemp "${TMPDIR_BASE}/flat-cyborg-install.XXXXXX")
SUMFILE="${TMPFILE}.sha256"
cleanup() { rm -f "$TMPFILE" "$SUMFILE"; }
trap cleanup EXIT INT TERM

fetch "${BASE_URL}/${BINARY}" "$TMPFILE"

# Verify download succeeded and is non-empty.
if [ ! -s "$TMPFILE" ]; then
  echo "Error: download failed or produced empty file" >&2
  echo "URL: ${BASE_URL}/${BINARY}" >&2
  exit 1
fi

# Verify the SHA256 checksum when possible.
if fetch "${BASE_URL}/${BINARY}.sha256" "$SUMFILE" 2>/dev/null && [ -s "$SUMFILE" ]; then
  EXPECTED=$(awk '{print $1}' "$SUMFILE")
  ACTUAL=$(sha256_of "$TMPFILE")
  if [ -z "$ACTUAL" ]; then
    echo "Warning: no sha256 tool found; skipping checksum verification" >&2
  elif [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "Error: checksum mismatch for $BINARY" >&2
    echo "  expected: $EXPECTED" >&2
    echo "  actual:   $ACTUAL" >&2
    exit 1
  else
    echo "Checksum verified."
  fi
else
  echo "Warning: checksum file unavailable; skipping verification" >&2
fi

chmod +x "$TMPFILE"

# Install (create the directory if needed, use sudo only if required).
DEST="$INSTALL_DIR/flat-cyborg"
if [ -w "$INSTALL_DIR" ] || [ -w "$(dirname "$INSTALL_DIR")" ]; then
  mkdir -p "$INSTALL_DIR"
  mv "$TMPFILE" "$DEST"
else
  echo "Installing to $INSTALL_DIR (requires sudo)..."
  sudo mkdir -p "$INSTALL_DIR"
  sudo mv "$TMPFILE" "$DEST"
fi
trap - EXIT INT TERM
rm -f "$SUMFILE"

echo "Installed: $DEST"
echo ""
echo "Get started:"
echo "  flat-cyborg --help"
echo "  flat-cyborg -- bash                          # wrap an interactive shell"
echo "  flat-cyborg --cmd 'echo hi' --cmd 'exit' -- sh -i   # drive commands"
