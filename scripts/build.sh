#!/bin/sh
set -eu

FINDER_ROOT=$(CDPATH='' cd "$(dirname "$0")/.." && pwd -P)
cd "$FINDER_ROOT"

FINDER_HOST=$(rustc -vV | sed -n 's/^host: //p')
test -n "$FINDER_HOST"

cargo build --manifest-path "$FINDER_ROOT/Cargo.toml" --locked --release \
    --bin herdr-finder --target "$FINDER_HOST" --target-dir "$FINDER_ROOT/target"

mkdir -p "$FINDER_ROOT/bin"
cp "$FINDER_ROOT/target/$FINDER_HOST/release/herdr-finder" "$FINDER_ROOT/bin/herdr-finder.new"
chmod 755 "$FINDER_ROOT/bin/herdr-finder.new"

mv "$FINDER_ROOT/bin/herdr-finder.new" "$FINDER_ROOT/bin/herdr-finder"
