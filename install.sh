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

# Verify the SHA256 checksum. This fails closed: a missing checksum file or a
# missing sha256 tool aborts the install, since this script is meant to be run
# as `curl ... | sh` (often with sudo) and an unverified binary is the prime
# attack target. Set FLAT_CYBORG_INSECURE=1 to install without verification.
INSECURE="${FLAT_CYBORG_INSECURE:-0}"
insecure_skip() {
  # $1: reason
  if [ "$INSECURE" = "1" ]; then
    echo "Warning: $1; installing WITHOUT verification (FLAT_CYBORG_INSECURE=1)" >&2
  else
    echo "Error: $1." >&2
    echo "Refusing to install an unverified binary. Re-run with FLAT_CYBORG_INSECURE=1 to override." >&2
    exit 1
  fi
}

if fetch "${BASE_URL}/${BINARY}.sha256" "$SUMFILE" 2>/dev/null && [ -s "$SUMFILE" ]; then
  EXPECTED=$(awk '{print $1}' "$SUMFILE")
  ACTUAL=$(sha256_of "$TMPFILE")
  if [ -z "$ACTUAL" ]; then
    insecure_skip "no sha256 tool (sha256sum/shasum) found"
  elif [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "Error: checksum mismatch for $BINARY" >&2
    echo "  expected: $EXPECTED" >&2
    echo "  actual:   $ACTUAL" >&2
    exit 1
  else
    echo "Checksum verified."
  fi
else
  insecure_skip "checksum file ${BINARY}.sha256 unavailable"
fi

chmod +x "$TMPFILE"

# True when $INSTALL_DIR can be created/written without sudo, i.e. its nearest
# existing ancestor is writable. Walking up (rather than checking only one
# level) avoids a needless sudo prompt for a deep custom install dir under a
# writable root.
can_install_without_sudo() {
  d="$1"
  while [ ! -e "$d" ]; do
    parent=$(dirname "$d")
    [ "$parent" = "$d" ] && break
    d="$parent"
  done
  [ -w "$d" ]
}

# Install (create the directory if needed, use sudo only if required).
DEST="$INSTALL_DIR/flat-cyborg"
if can_install_without_sudo "$INSTALL_DIR"; then
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
