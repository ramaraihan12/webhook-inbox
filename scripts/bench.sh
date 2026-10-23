#!/usr/bin/env bash
# Load runs against the compose stack, using oha from Docker on the compose
# network (so no Docker Desktop port forwarding sits in the path).
#
#   SINK_QUIET=1 docker compose up -d --build
#   scripts/bench.sh receive     # signed github-style payload, every request stored
#   scripts/bench.sh duplicate   # same event id every time, answered as a duplicate
#   scripts/bench.sh rejected    # wrong signature, answered 401
#
# DURATION (default 30s) and CONC (default 50) change the run. OHA_ARGS adds
# flags to oha, for example OHA_ARGS="-q 50" to cap it at 50 requests a second.
set -euo pipefail
export MSYS_NO_PATHCONV=1

mode="${1:-receive}"
net="${NET:-webhook-inbox_default}"
secret="${GITHUB_WEBHOOK_SECRET:-demo-github-secret}"
duration="${DURATION:-30s}"
conc="${CONC:-50}"

body='{"action":"opened","pull_request":{"number":42,"title":"Tambah validasi nomor rekening"},"repository":{"full_name":"kabut-labs/billing-api"}}'
sig="sha256=$(printf '%s' "$body" | openssl dgst -sha256 -hmac "$secret" -hex | sed 's/^.* //')"

case "$mode" in
  receive) source=bench ;;
  duplicate) source=github ;;
  rejected) source=bench; sig="sha256=$(printf '%064d' 0)" ;;
  *) echo "usage: $0 receive|duplicate|rejected" >&2; exit 2 ;;
esac

# The duplicate run needs the first copy stored, the rest are then duplicates.
extra=()
if [ "$mode" = duplicate ]; then
  extra=(-H "x-github-delivery: bench-fixed-id")
fi

docker run --rm --network "$net" ghcr.io/hatoo/oha:1.16.0 \
  --no-tui -z "$duration" -c "$conc" -m POST ${OHA_ARGS:-} \
  -H "content-type: application/json" \
  -H "x-hub-signature-256: $sig" \
  ${extra[@]+"${extra[@]}"} \
  -d "$body" \
  "http://inbox:8020/in/$source"
