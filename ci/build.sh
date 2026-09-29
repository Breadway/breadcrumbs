#!/usr/bin/env bash
# Delegates to bread-ecosystem's shared CI build image/script, pinned to
# the commit in ci/bread-ecosystem.rev, not `main`. bread-ecosystem's CI
# files now affect every product's release pipeline, so bumping the pin
# is a deliberate act instead of silent drift (see the bread-theme test
# that broke here for exactly that reason, before it was pinned by rev).
#
# Usage: ci/build.sh cargo build --release --locked
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REV="$(cat "${ROOT}/ci/bread-ecosystem.rev")"

# The runner's /tmp is shared by concurrent jobs, so the checkout must never
# be half-built or deleted under another job (a glob `rm -rf
# /tmp/bread-ecosystem-ci-*` here used to do exactly that):
#   - it is cloned into a private temp dir, marked .ready, then renamed into
#     place, so a job sees either a complete checkout or none;
#   - cloning takes a lock, so two jobs never clone the same rev at once;
#   - checkouts for other revs are only pruned once unused for a day. Every
#     run touches its own, so anything a job is using stays fresh.
CI_TMP="${BREAD_CI_TMP:-/tmp}"
CACHE_DIR="${CI_TMP}/bread-ecosystem-ci-${REV}"
if [ ! -f "${CACHE_DIR}/.ready" ]; then
    if command -v flock >/dev/null 2>&1; then
        exec 8>"${CI_TMP}/bread-ecosystem-ci.lock"
        flock -w 600 8 || { echo "build.sh: timed out waiting for the bread-ecosystem clone lock" >&2; exit 1; }
    fi
    if [ ! -f "${CACHE_DIR}/.ready" ]; then
        CLONE_TMP="$(mktemp -d "${CI_TMP}/bread-ecosystem-ci-clone-XXXXXX")"
        git clone --quiet "${BREAD_ECOSYSTEM_URL:-https://git.breadway.dev/Breadway/bread-ecosystem.git}" "$CLONE_TMP"
        git -C "$CLONE_TMP" checkout --quiet "$REV"
        touch "${CLONE_TMP}/.ready"
        rm -rf "$CACHE_DIR"
        mv "$CLONE_TMP" "$CACHE_DIR"
        find "$CI_TMP" -maxdepth 1 -name 'bread-ecosystem-ci-*' -mmin +1440 -exec rm -rf {} + 2>/dev/null || true
    fi
    exec 8>&-
fi
touch "$CACHE_DIR" 2>/dev/null || true

bash "${CACHE_DIR}/ci/build.sh" breadcrumbs "$ROOT" "$@"
