#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Create the Fedora source tarballs, vendor archive and SRPM for a release tag.
#
# Usage:
#   ./packaging/fedora/make-sources.sh [TAG] [OUTDIR] [--submit]
#
# TAG defaults to the version recorded in beyrix.spec. OUTDIR defaults to
# dist/fedora. With --submit the generated SRPM is uploaded to the COPR
# project configured in ~/.config/copr.
#
# The vendor archive is produced with `cargo vendor --locked` from the Cargo.lock
# checked into the tag, so builds are offline and dependency versions are fixed.

set -Eeuo pipefail

ROOT=$(git rev-parse --show-toplevel)
HERE="$ROOT/packaging/fedora"
SPEC="$HERE/beyrix.spec"

SPEC_UPVER=$(sed -n 's/^%global upver //p' "$SPEC")
TAG="${1:-v$SPEC_UPVER}"
OUTDIR="${2:-$ROOT/dist/fedora}"
SUBMIT=0
for arg in "$@"; do
    case "$arg" in
        --submit) SUBMIT=1 ;;
    esac
done
if [[ $OUTDIR == --submit ]]; then
    OUTDIR="$ROOT/dist/fedora"
fi
TAG="${TAG#v}"
UPVER="$TAG"

COPR_PROJECT="${COPR_PROJECT:-beykus-y/beyrix}"

if ! command -v cargo >/dev/null; then
    echo "error: cargo is required to generate the vendor archive" >&2
    exit 1
fi
if ! command -v rpmbuild >/dev/null; then
    echo "error: rpmbuild is required (dnf install rpm-build)" >&2
    exit 1
fi

mkdir -p "$OUTDIR"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/beyrix-sources.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

SRC="$OUTDIR/Beyrix-$UPVER.tar.gz"
VENDOR="$OUTDIR/beyrix-vendor-$UPVER.tar.gz"

if [[ ! -f $SRC ]]; then
    echo "Downloading source archive for v$UPVER"
    curl -fL --retry 3 -o "$SRC.tmp" \
        "https://github.com/Beykus-Y/Beyrix/archive/refs/tags/v$UPVER.tar.gz"
    mv "$SRC.tmp" "$SRC"
fi
sha256sum "$SRC" | tee "$SRC.sha256"

# Use the tag's commit timestamp for reproducible archives when possible.
EPOCH=${SOURCE_DATE_EPOCH:-}
if [[ -z $EPOCH ]] && git -C "$ROOT" rev-parse -q --verify "v$UPVER^{commit}" >/dev/null; then
    EPOCH=$(git -C "$ROOT" log -1 --format=%ct "v$UPVER")
fi
if [[ -z $EPOCH ]]; then
    EPOCH=$(curl -fsSL "https://api.github.com/repos/Beykus-Y/Beyrix/commits/v$UPVER" \
        | sed -n 's/.*"date": "\([0-9T:Z-]*\)".*/\1/p' | head -n1)
    EPOCH=$(date -u -d "$EPOCH" +%s)
fi
echo "Using SOURCE_DATE_EPOCH=$EPOCH"

tar -xzf "$SRC" -C "$WORK"
SRCDIR="$WORK/Beyrix-$UPVER"
if [[ ! -d $SRCDIR ]]; then
    echo "error: unexpected archive layout, missing Beyrix-$UPVER" >&2
    exit 1
fi

(
    cd "$SRCDIR"
    cargo vendor --locked vendor >/dev/null
)
# Keep the vendor tree free of files that are not part of the crates.
rm -rf "$SRCDIR/vendor/.git"

tar --sort=name --mtime="@$EPOCH" --owner=0 --group=0 --numeric-owner \
    -C "$SRCDIR" -cf - vendor | gzip -9n > "$VENDOR"
sha256sum "$VENDOR" | tee "$VENDOR.sha256"

install -m 644 "$HERE/io.github.beykus_y.Beyrix.desktop" "$OUTDIR/"
install -m 644 "$HERE/io.github.beykus_y.Beyrix.metainfo.xml" "$OUTDIR/"
install -m 644 "$HERE/beyrix.1" "$OUTDIR/"
install -m 644 "$SPEC" "$OUTDIR/"

rpmbuild -bs "$SPEC" \
    --define "_sourcedir $OUTDIR" \
    --define "_srcrpmdir $OUTDIR" \
    --define "_topdir $WORK/rpmbuild" \
    --define "use_source_date_epoch_as_buildtime 1" \
    --define "clamp_mtime_to_source_date_epoch 1"

SRPM=$(find "$OUTDIR" -maxdepth 1 -name "beyrix-*.src.rpm" -printf '%T@ %p\n' | sort -n | tail -n1 | cut -d' ' -f2-)
if [[ -z $SRPM ]]; then
    echo "error: SRPM was not created" >&2
    exit 1
fi
echo "Created $SRPM"

if [[ $SUBMIT -eq 1 ]]; then
    copr-cli build "$COPR_PROJECT" "$SRPM"
fi