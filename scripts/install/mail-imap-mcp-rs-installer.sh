#!/bin/sh
set -eu

REPO="bradsjm/mail-imap-mcp-rs"
BIN_NAME="mail-imap-mcp-rs"
DEFAULT_INSTALL_DIR="$HOME/.local/bin"
VERSION="__VERSION__"

usage() {
  cat <<'EOF'
Install mail-imap-mcp-rs from GitHub Releases.

Usage:
  sh mail-imap-mcp-rs-installer.sh [--version vX.Y.Z] [--dir /path]

Options:
  --version   Release tag (for example: v0.1.0)
  --dir       Install directory (default: ~/.local/bin)
  --help      Show this help message

Environment:
  INSTALL_DIR  Install directory override
EOF
}

has_cmd() {
  command -v "$1" >/dev/null 2>&1
}

download_to() {
  url="$1"
  out="$2"
  if has_cmd curl; then
    curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$out"
    return 0
  fi
  if has_cmd wget; then
    wget -qO "$out" "$url"
    return 0
  fi
  echo "error: neither curl nor wget is available" >&2
  exit 1
}

verify_checksum() {
  checksums_file="$1"
  asset_file="$2"
  asset_name="$3"

  if has_cmd sha256sum; then
    grep "  $asset_name$" "$checksums_file" | sha256sum -c - >/dev/null
    return 0
  fi

  if has_cmd shasum; then
    expected="$(grep "  $asset_name$" "$checksums_file" | awk '{print $1}')"
    actual="$(shasum -a 256 "$asset_file" | awk '{print $1}')"
    [ "$expected" = "$actual" ]
    return 0
  fi

  echo "error: neither sha256sum nor shasum is available" >&2
  exit 1
}

detect_target_candidates() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux)
      case "$arch" in
        x86_64|amd64)
          echo "${BIN_NAME}-linux-x64.tar.gz"
          ;;
        *)
          echo "error: unsupported Linux architecture: $arch" >&2
          exit 1
          ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        x86_64)
          echo "${BIN_NAME}-darwin-x64.tar.gz"
          ;;
        arm64|aarch64)
          echo "${BIN_NAME}-darwin-arm64.tar.gz"
          ;;
        *)
          echo "error: unsupported macOS architecture: $arch" >&2
          exit 1
          ;;
      esac
      ;;
    *)
      echo "error: unsupported OS: $os" >&2
      exit 1
      ;;
  esac
}

INSTALL_DIR="${INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || {
        echo "error: --version requires a value" >&2
        exit 1
      }
      VERSION="$2"
      shift 2
      ;;
    --dir)
      [ "$#" -ge 2 ] || {
        echo "error: --dir requires a value" >&2
        exit 1
      }
      INSTALL_DIR="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage
      exit 1
      ;;
  esac
done

case "$VERSION" in
  __VERSION__)
    echo "error: installer version is not set; pass --version vX.Y.Z" >&2
    exit 1
    ;;
  v*)
    ;;
  *)
    echo "error: version must look like vX.Y.Z (got: $VERSION)" >&2
    exit 1
    ;;
esac

if [ ! -d "$INSTALL_DIR" ]; then
  mkdir -p "$INSTALL_DIR"
fi

if [ ! -w "$INSTALL_DIR" ]; then
  echo "error: install directory is not writable: $INSTALL_DIR" >&2
  echo "hint: choose a writable directory with --dir or INSTALL_DIR" >&2
  exit 1
fi

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
CHECKSUMS_FILE="$TMP_DIR/checksums.txt"
download_to "$BASE_URL/checksums.txt" "$CHECKSUMS_FILE"

ASSET=""
for candidate in $(detect_target_candidates); do
  if grep -q "  $candidate$" "$CHECKSUMS_FILE"; then
    ASSET="$candidate"
    break
  fi
done

if [ -z "$ASSET" ]; then
  echo "error: no matching release asset found for this platform in $VERSION" >&2
  exit 1
fi

ARCHIVE_PATH="$TMP_DIR/$ASSET"
download_to "$BASE_URL/$ASSET" "$ARCHIVE_PATH"
verify_checksum "$CHECKSUMS_FILE" "$ARCHIVE_PATH" "$ASSET"

tar -xzf "$ARCHIVE_PATH" -C "$TMP_DIR"

if [ ! -f "$TMP_DIR/$BIN_NAME" ]; then
  echo "error: archive did not contain expected binary '$BIN_NAME'" >&2
  exit 1
fi

cp "$TMP_DIR/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
chmod +x "$INSTALL_DIR/$BIN_NAME"

echo "installed: $INSTALL_DIR/$BIN_NAME"
echo "run: $BIN_NAME"
