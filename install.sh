#!/bin/sh
set -eu

REPOSITORY=Kodade/kodade-cli
INSTALL_DIR=${KODADE_INSTALL_DIR:-${HOME:-}/.local/bin}

if [ -z "${HOME:-}" ] && [ -z "${KODADE_INSTALL_DIR:-}" ]; then
    echo "error: HOME is not set; use KODADE_INSTALL_DIR to choose an install directory" >&2
    exit 1
fi

command -v curl >/dev/null 2>&1 || {
    echo "error: curl is required" >&2
    exit 1
}
command -v tar >/dev/null 2>&1 || {
    echo "error: tar is required" >&2
    exit 1
}

OS=$(uname -s)
ARCH=$(uname -m)
case "$OS:$ARCH" in
    Darwin:arm64|Darwin:aarch64)
        TARGET=aarch64-apple-darwin
        ;;
    Darwin:x86_64|Darwin:amd64)
        TARGET=x86_64-apple-darwin
        ;;
    Linux:arm64|Linux:aarch64)
        TARGET=aarch64-unknown-linux-gnu
        ;;
    Linux:x86_64|Linux:amd64)
        TARGET=x86_64-unknown-linux-gnu
        ;;
    Linux:*)
        echo "error: unsupported Linux architecture: $ARCH" >&2
        exit 1
        ;;
    Darwin:*)
        echo "error: unsupported macOS architecture: $ARCH" >&2
        exit 1
        ;;
    MINGW*:*|MSYS*:*|CYGWIN*:*)
        echo "error: Windows is not supported directly; use WSL" >&2
        exit 1
        ;;
    *)
        echo "error: unsupported operating system: $OS (Windows users should use WSL)" >&2
        exit 1
        ;;
esac

TMP_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t kodade-cli)
trap 'rm -rf "$TMP_DIR"' EXIT HUP INT TERM

API_URL="https://api.github.com/repos/$REPOSITORY/releases/latest"
RELEASE_JSON="$TMP_DIR/release.json"
curl -fsSL -H 'Accept: application/vnd.github+json' "$API_URL" -o "$RELEASE_JSON"
TAG=$(sed -n 's/^[[:space:]]*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$RELEASE_JSON" | head -n 1)
case "$TAG" in
    v[0-9]*) VERSION=${TAG#v} ;;
    *) echo "error: latest GitHub release did not provide a valid version tag" >&2; exit 1 ;;
esac

ARCHIVE_NAME="kodade-cli-${VERSION}-${TARGET}.tar.gz"
BASE_URL="https://github.com/$REPOSITORY/releases/download/$TAG"
ARCHIVE="$TMP_DIR/$ARCHIVE_NAME"
SUMS="$TMP_DIR/SHA256SUMS"
curl -fsSL "$BASE_URL/$ARCHIVE_NAME" -o "$ARCHIVE"
curl -fsSL "$BASE_URL/SHA256SUMS" -o "$SUMS"

CHECKSUM=$(awk -v file="$ARCHIVE_NAME" '$2 == file { print $1; exit }' "$SUMS")
if [ -z "$CHECKSUM" ]; then
    echo "error: no checksum found for $ARCHIVE_NAME" >&2
    exit 1
fi
case "$CHECKSUM" in
    *[!0123456789abcdefABCDEF]*|'')
        echo "error: invalid checksum for $ARCHIVE_NAME" >&2
        exit 1
        ;;
esac
if [ "$(printf '%s' "$CHECKSUM" | awk '{ print length($0) }')" -ne 64 ]; then
    echo "error: invalid checksum length for $ARCHIVE_NAME" >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "$CHECKSUM" "$ARCHIVE" | sha256sum -c -
elif command -v shasum >/dev/null 2>&1; then
    printf '%s  %s\n' "$CHECKSUM" "$ARCHIVE" | shasum -a 256 -c -
else
    echo "error: sha256sum or shasum is required to verify the download" >&2
    exit 1
fi

mkdir -p "$INSTALL_DIR"
tar -xzf "$ARCHIVE" -C "$TMP_DIR"
EXTRACTED="$TMP_DIR/kodade-cli-${VERSION}-${TARGET}/kodade-cli"
if [ ! -f "$EXTRACTED" ]; then
    echo "error: release archive did not contain kodade-cli" >&2
    exit 1
fi
chmod 755 "$EXTRACTED"
mv "$EXTRACTED" "$INSTALL_DIR/kodade-cli"

echo "Installed kodade-cli ${VERSION} to $INSTALL_DIR/kodade-cli"
case ":${PATH:-}:" in
    *:"$INSTALL_DIR":*) ;;
    *) echo "Add $INSTALL_DIR to PATH: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
