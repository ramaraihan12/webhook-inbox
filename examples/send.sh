#!/usr/bin/env bash
# Sends one made-up, correctly signed webhook to a running inbox.
#
#   examples/send.sh github
#   examples/send.sh midtrans
#   examples/send.sh xendit
#   examples/send.sh partner
#   examples/send.sh bad       # github payload with the wrong secret, gets 401
#
# The secrets default to the demo values in docker-compose.yml.
set -euo pipefail

BASE="${BASE:-http://127.0.0.1:8020}"
GITHUB_WEBHOOK_SECRET="${GITHUB_WEBHOOK_SECRET:-demo-github-secret}"
MIDTRANS_SERVER_KEY="${MIDTRANS_SERVER_KEY:-SB-Mid-server-demo}"
XENDIT_CALLBACK_TOKEN="${XENDIT_CALLBACK_TOKEN:-demo-xendit-token}"
PARTNER_SECRET="${PARTNER_SECRET:-demo-partner-secret}"

uid="$(date +%s)-$RANDOM"
hmac_hex() { printf '%s' "$2" | openssl dgst -sha256 -hmac "$1" -hex | sed 's/^.* //'; }
sha512_hex() { printf '%s' "$1" | openssl dgst -sha512 -hex | sed 's/^.* //'; }

post() { # source, then curl args
  local source="$1"
  shift
  curl -s -i -X POST "$BASE/in/$source" -H 'content-type: application/json' "$@" | sed -n '1p;/^x-inbox/p;$p'
  echo
}

case "${1:-github}" in
  github | bad)
    body='{"action":"opened","pull_request":{"number":42,"title":"Tambah validasi nomor rekening"},"repository":{"full_name":"kabut-labs/billing-api"}}'
    secret="$GITHUB_WEBHOOK_SECRET"
    [ "$1" = bad ] && secret="not-the-secret"
    sig="sha256=$(hmac_hex "$secret" "$body")"
    post github -H "x-hub-signature-256: $sig" -H "x-github-delivery: gh-$uid" -H 'x-github-event: pull_request' -d "$body"
    ;;
  midtrans)
    order="KBT-$uid"
    sig="$(sha512_hex "${order}200150000.00${MIDTRANS_SERVER_KEY}")"
    body="{\"order_id\":\"$order\",\"transaction_id\":\"trx-$uid\",\"transaction_status\":\"settlement\",\"status_code\":\"200\",\"gross_amount\":\"150000.00\",\"payment_type\":\"qris\",\"signature_key\":\"$sig\"}"
    post midtrans -d "$body"
    ;;
  xendit)
    body="{\"id\":\"inv-$uid\",\"external_id\":\"INV-KBT-0042\",\"status\":\"PAID\",\"amount\":275000,\"paid_at\":\"2026-10-23T09:41:00Z\"}"
    post xendit -H "x-callback-token: $XENDIT_CALLBACK_TOKEN" -d "$body"
    ;;
  partner)
    body="{\"event\":\"shipment.created\",\"po\":\"PO-2026-117\",\"qty\":24}"
    t="$(date +%s)"
    sig="$(hmac_hex "$PARTNER_SECRET" "$t.$body")"
    post partner -H "x-partner-signature: t=$t,v1=$sig" -H "x-partner-event: pe-$uid" -d "$body"
    ;;
  *)
    echo "usage: $0 github|midtrans|xendit|partner|bad" >&2
    exit 2
    ;;
esac
