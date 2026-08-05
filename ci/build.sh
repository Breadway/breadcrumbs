#!/usr/bin/env bash
# Delegates to bread-ecosystem's shared CI build image/script, pinned to
# the commit in ci/bread-ecosystem.rev — not `main`. bread-ecosystem's CI
# files now affect every product's release pipeline, so bumping the pin
# is a deliberate act instead of silent drift (see the bread-theme test
# that broke here for exactly that reason, before it was pinned by rev).
#
# Usage: ci/build.sh cargo build --release --locked
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REV="$(cat "${ROOT}/ci/bread-ecosystem.rev")"

CACHE_DIR="/tmp/bread-ecosystem-ci-${REV}"
if [ ! -d "$CACHE_DIR" ]; then
    rm -rf /tmp/bread-ecosystem-ci-*
    git clone https://git.breadway.dev/Breadway/bread-ecosystem.git "$CACHE_DIR"
    git -C "$CACHE_DIR" checkout --quiet "$REV"
fi

bash "${CACHE_DIR}/ci/build.sh" breadcrumbs "$ROOT" "$@"
