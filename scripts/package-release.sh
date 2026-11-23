#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Package an already-built release binary from the repository root.
# Usage: ./scripts/package-release.sh v0.1.0-alpha.1

set -Eeuo pipefail

cd "$(dirname "$0")/.."

TAG="${1:-}"
if [[ -z $TAG ]]; then
    echo "usage: $0 TAG (for example v0.1.0-alpha.1)" >&2
    exit 2
fi
if [[ ! $TAG =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "error: tag must look like v1.2.3 or v1.2.3-alpha.1" >&2
    exit 2
fi

EXPECTED="${TAG#v}"
MANIFEST_VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
if [[ $MANIFEST_VERSION != "$EXPECTED" ]]; then
    echo "error: Cargo.toml version is '$MANIFEST_VERSION', tag expects '$EXPECTED'" >&2
    exit 1
fi

BIN="target/release/beyrix"
if [[ ! -x $BIN ]]; then
    echo "error: $BIN is missing; run cargo build --release --locked first" >&2
    exit 1
fi

COMMIT=$(git rev-parse --verify HEAD)
if [[ -n $(git status --porcelain) ]]; then
    echo "warning: working tree has uncommitted or untracked changes; archive is not a clean release" >&2
fi

NAME="beyrix-${TAG}-linux-x86_64"
DIST="dist"
STAGE="$DIST/$NAME"
ARCHIVE="$DIST/$NAME.tar.gz"
EPOCH=$(git log -1 --format=%ct)

rm -rf "$STAGE"
mkdir -p "$STAGE/docs"

install -m 755 "$BIN" "$STAGE/beyrix"
install -m 644 LICENSE README.md CONFIGURATION.en.md CONFIGURATION.md "$STAGE/"
install -m 644 docs/ARCHITECTURE.md "$STAGE/docs/ARCHITECTURE.md"
install -m 755 packaging/install.sh "$STAGE/install.sh"

cat > "$STAGE/BUILD-INFO.txt" <<EOF
Beyrix $TAG
Source commit: $COMMIT
Target: x86_64-unknown-linux-gnu
Build platform: $(uname -srm)
Rust: $(rustc --version)
Cargo: $(cargo --version)
Built at: $(date -u +%Y-%m-%dT%H:%M:%SZ)
EOF

tar --sort=name --mtime="@$EPOCH" --owner=0 --group=0 --numeric-owner \
    -C "$DIST" -cf - "$NAME" | gzip -9n > "$ARCHIVE"

(
    cd "$DIST"
    sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256"
    sha256sum "$NAME.tar.gz" > SHA256SUMS
)

echo "Created:"
echo "  $ARCHIVE ($(du -h "$ARCHIVE" | cut -f1))"
echo "  $DIST/SHA256SUMS"
cat "$DIST/SHA256SUMS"