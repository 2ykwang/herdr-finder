#!/bin/sh
set -eu

FINDER_ROOT=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
FINDER_VERSION=$(awk -F '"' '/^version[[:space:]]*=/ { print $2; exit }' "$FINDER_ROOT/herdr-plugin.toml")
case "$FINDER_VERSION" in
    ''|*[!0-9A-Za-z.+-]*) printf '%s\n' 'Invalid plugin version.' >&2; exit 1 ;;
esac

case "$(uname -s)" in
    Darwin) FINDER_PLATFORM=apple-darwin ;;
    Linux) FINDER_PLATFORM=unknown-linux-musl ;;
    *) printf '%s\n' 'Supported platforms: macOS and Linux.' >&2; exit 1 ;;
esac
case "$(uname -m)" in
    arm64|aarch64) FINDER_ARCH=aarch64 ;;
    x86_64|amd64) FINDER_ARCH=x86_64 ;;
    *) printf '%s\n' 'Supported architectures: x86_64 and ARM64.' >&2; exit 1 ;;
esac

for FINDER_TOOL in curl tar mktemp; do
    command -v "$FINDER_TOOL" >/dev/null 2>&1 || {
        printf 'Required command: %s\n' "$FINDER_TOOL" >&2
        exit 1
    }
done

FINDER_BASE="herdr-finder-$FINDER_VERSION"
FINDER_ARCHIVE="$FINDER_BASE-$FINDER_ARCH-$FINDER_PLATFORM.tar.gz"
FINDER_URL="https://github.com/2ykwang/herdr-finder/releases/download/v$FINDER_VERSION"
FINDER_TEMP=$(mktemp -d "${TMPDIR:-/tmp}/herdr-finder.XXXXXX")
FINDER_STAGE=''
cleanup() {
    rm -rf "$FINDER_TEMP"
    if [ -n "$FINDER_STAGE" ]; then rm -f "$FINDER_STAGE"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

download() {
    curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
        --connect-timeout 10 --max-time 120 --retry 2 --output "$2" "$1" || {
        printf 'Could not download release v%s. Publish its platform assets first.\n' "$FINDER_VERSION" >&2
        exit 1
    }
}

download "$FINDER_URL/$FINDER_ARCHIVE" "$FINDER_TEMP/archive.tar.gz"
download "$FINDER_URL/$FINDER_ARCHIVE.sha256" "$FINDER_TEMP/checksum"
FINDER_EXPECTED=$(awk 'NR == 1 && NF == 2 { print $1 }' "$FINDER_TEMP/checksum")
FINDER_FILENAME=$(awk 'NR == 1 && NF == 2 { print $2 }' "$FINDER_TEMP/checksum")
case "$FINDER_EXPECTED" in
    ''|*[!0-9a-fA-F]*) printf '%s\n' 'Invalid SHA-256 checksum.' >&2; exit 1 ;;
esac
if [ "${#FINDER_EXPECTED}" -ne 64 ] || [ "$FINDER_FILENAME" != "$FINDER_ARCHIVE" ]; then
    printf '%s\n' 'The checksum does not identify the requested archive.' >&2
    exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
    FINDER_ACTUAL=$(sha256sum "$FINDER_TEMP/archive.tar.gz" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    FINDER_ACTUAL=$(shasum -a 256 "$FINDER_TEMP/archive.tar.gz" | awk '{print $1}')
else
    printf '%s\n' 'Required command: sha256sum or shasum.' >&2
    exit 1
fi
if [ "$FINDER_ACTUAL" != "$FINDER_EXPECTED" ]; then
    printf '%s\n' 'SHA-256 verification failed. The installed binary was kept.' >&2
    exit 1
fi

FINDER_MEMBER="$FINDER_BASE/bin/herdr-finder"
tar -xzf "$FINDER_TEMP/archive.tar.gz" -C "$FINDER_TEMP" "$FINDER_MEMBER"
FINDER_BINARY="$FINDER_TEMP/$FINDER_MEMBER"
test -f "$FINDER_BINARY" && test ! -L "$FINDER_BINARY"
chmod 755 "$FINDER_BINARY"
if [ "$("$FINDER_BINARY" --version)" != "$FINDER_VERSION" ]; then
    printf '%s\n' 'The downloaded binary has the wrong version.' >&2
    exit 1
fi
mkdir -p "$FINDER_ROOT/bin"
FINDER_STAGE=$(mktemp "$FINDER_ROOT/bin/.herdr-finder.XXXXXX")
cp "$FINDER_BINARY" "$FINDER_STAGE"
chmod 755 "$FINDER_STAGE"
mv "$FINDER_STAGE" "$FINDER_ROOT/bin/herdr-finder"
FINDER_STAGE=''
printf 'Installed Herdr Finder %s (%s-%s).\n' "$FINDER_VERSION" "$FINDER_ARCH" "$FINDER_PLATFORM"
