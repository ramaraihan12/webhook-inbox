#!/usr/bin/env sh
# Run cargo inside the official Rust image, for machines without a Rust toolchain.
# Caches live in named volumes. Database tests run only when TEST_DATABASE_URL is
# set, and the URL is used as given. A Postgres on the host is reachable from the
# container as host.docker.internal:
#
#   TEST_DATABASE_URL=postgresql://postgres:postgres@host.docker.internal:5432/webhook_inbox_test \
#     scripts/cargo-docker.sh test
#
# Without it the database tests are skipped.
#
#   scripts/cargo-docker.sh test
#   scripts/cargo-docker.sh clippy --all-targets -- -D warnings
set -eu
# Git Bash on Windows would rewrite the container paths below.
export MSYS_NO_PATHCONV=1

BASE="${RUST_IMAGE:-rust:1.99-bookworm}"
DEV="webhook-inbox-dev"

# The base image has no rustfmt or clippy, so add them once.
if ! docker image inspect "$DEV" >/dev/null 2>&1; then
  printf 'FROM %s\nRUN rustup component add rustfmt clippy\n' "$BASE" | docker build -t "$DEV" -
fi

exec docker run --rm \
  -v "$PWD":/work -w /work \
  -v webhook-inbox-cargo-registry:/usr/local/cargo/registry \
  -v webhook-inbox-target:/target \
  -e CARGO_TARGET_DIR=/target \
  -e CARGO_BUILD_JOBS=6 \
  ${TEST_DATABASE_URL:+-e TEST_DATABASE_URL="$TEST_DATABASE_URL"} \
  --add-host host.docker.internal:host-gateway \
  "$DEV" cargo "$@"
