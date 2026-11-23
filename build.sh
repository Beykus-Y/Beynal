#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

set -Eeuo pipefail

cd "$(dirname "$0")"

MODE="${1:-build}"

format() {
    if cargo fmt --version >/dev/null 2>&1; then
        cargo fmt --all -- --check
    else
        echo "Rustfmt не установлен. Установи: sudo dnf install rustfmt"
        exit 1
    fi
}

check() {
    cargo check --all-targets
}

case "$MODE" in
    build)
        format
        check
        cargo build
        ;;
    release)
        format
        check
        cargo build --release
        ;;
    run)
        format
        check
        cargo run
        ;;
    clean)
        cargo clean
        ;;
    fmt)
        cargo fmt --all
        ;;
    *)
        echo "Использование: ./build.sh [build|release|run|clean|fmt]"
        exit 2
        ;;
esac