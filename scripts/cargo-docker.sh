#!/usr/bin/env sh
# Run cargo inside the official Rust image, for machines without a Rust toolchain.
# Caches live in named volumes. The database is reached on the host at port 5433.
#
#   scripts/cargo-docker.sh test
#   scripts/cargo-docker.sh clippy --all-targets -- -D warnings
set -eu
# Git Bash on Windows would rewrite the container paths below.
export MSYS_NO_PATHCONV=1

IMAGE="${RUST_IMAGE:-rust:1.99-bookworm}"
TEST_DB="${TEST_DATABASE_URL:-postgresql://postgres:postgres@host.docker.internal:5433/webhook_inbox_test}"

exec docker run --rm \
  -v "$PWD":/work -w /work \
  -v webhook-inbox-cargo-registry:/usr/local/cargo/registry \
  -v webhook-inbox-target:/target \
  -e CARGO_TARGET_DIR=/target \
  -e CARGO_BUILD_JOBS=6 \
  -e TEST_DATABASE_URL="$TEST_DB" \
  --add-host host.docker.internal:host-gateway \
  "$IMAGE" cargo "$@"
