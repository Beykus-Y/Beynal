#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

set -eu

PREFIX="${PREFIX:-$HOME/.local}"
BINDIR="$PREFIX/bin"

if [ "$#" -gt 0 ]; then
    echo "usage: PREFIX=/usr/local ./install.sh" >&2
    exit 2
fi

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ ! -x "$here/beyrix" ]; then
    echo "install.sh: beyrix binary not found next to this script" >&2
    exit 1
fi

install -d "$BINDIR"
if [ -e "$BINDIR/bedit" ] || [ -L "$BINDIR/bedit" ]; then
    if [ "$(readlink "$BINDIR/bedit" || true)" != "beyrix" ]; then
        echo "install.sh: $BINDIR/bedit already exists and is not the Beyrix link" >&2
        exit 1
    fi
fi
install -m 755 "$here/beyrix" "$BINDIR/beyrix"
if [ ! -L "$BINDIR/bedit" ]; then
    ln -s beyrix "$BINDIR/bedit"
fi

install -Dm644 "$here/io.github.beykus_y.BEdit.desktop" "$PREFIX/share/applications/io.github.beykus_y.BEdit.desktop"

echo "Installed beyrix and bedit to $BINDIR"
case ":${PATH:-}:" in
    *":$BINDIR:"*) ;;
    *) echo "Add $BINDIR to PATH, then run: beyrix" ;;
esac