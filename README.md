# webhook-inbox

[![ci](https://github.com/ramaraihan12/webhook-inbox/actions/workflows/ci.yml/badge.svg)](https://github.com/ramaraihan12/webhook-inbox/actions/workflows/ci.yml)

A small self-hosted service in Rust that receives webhooks from Midtrans, Xendit, GitHub or any HMAC signer, checks the signature, stores every delivery in Postgres and forwards it to your app with retries. A web page at `/ui` lets you look at what arrived, see every attempt, and replay a delivery.

I wrote it for the case where a payment callback hits your app while it is down or mid-deploy and the provider gives up after a few tries. Your app can be offline for an hour and the inbox keeps the body and delivers it later. It costs nothing to run: one binary and one Postgres. All demo data below (Kabut Labs, order ids, amounts) is made up.

![The delivery list with filters for source and status, showing made-up GitHub, Midtrans, Xendit and partner deliveries](docs/deliveries.png)

A real run against the compose stack, one good GitHub-style delivery and one with the wrong secret:

```console
$ examples/send.sh github
HTTP/1.1 200 OK
x-inbox-delivery-id: e33889bc-8690-46b8-956a-81eb12cd2143
{"status":"accepted","id":"e33889bc-8690-46b8-956a-81eb12cd2143"}
$ examples/send.sh bad
HTTP/1.1 401 Unauthorized
{"error":"signature check failed"}
```

The 401 is stored too, as a rejected delivery (only the newest 500 are kept), so you can see what a provider really sent when your secret is wrong.

![Delivery detail for a Midtrans payment: two targets, a timeline with two failed tries and a third that succeeded, then the pretty-printed body and headers](docs/delivery-detail.png)

This Midtrans delivery has two targets. `billing` is a demo endpoint that fails twice, so the timeline shows try 1 (HTTP 500), try 2 (HTTP 500) and try 3 (HTTP 200) with the backoff between them. `audit` worked on the first try.

![Dead-letter view listing two partner deliveries that used all 5 tries against a target that answers 503, each with a Replay button](docs/dead-letters.png)

## How it works

- `inbox.toml` lists sources. Each one has a path `/in/{source}`, a verifier, the name of the env var that holds its secret, how to find the provider's event id (a header or JSON pointers) and one or more targets with a URL, extra headers and a timeout. `check-config` loads and prints it.
- Verifiers: GitHub (`X-Hub-Signature-256`), generic HMAC-SHA256 with a configurable header and an optional Stripe-style `t=...,v1=...` timestamp tolerance, Midtrans (sha512 of `order_id + status_code + gross_amount + server_key` against `signature_key` in the body) and Xendit (the static `x-callback-token`). Every comparison goes through `subtle`, so it is constant time. The last two prove less than the first two, see "What verified means" below.
- A verified request has its raw body, headers and result stored, plus one job per target, and the answer goes out before any forwarding. A second delivery with the same event id gets `200` with `x-inbox-duplicate: true` and no new jobs. Credentials such as `authorization` and `x-callback-token` are not stored.
- Workers claim jobs with `SELECT ... FOR UPDATE SKIP LOCKED` and a lease, so several instances can share one database. Each attempt POSTs the original body with `x-inbox-delivery-id`, `x-inbox-attempt` and `x-inbox-verified` (what the check covered). A 2xx ends the job. Anything else retries with exponential backoff and full jitter (defaults: 8 tries, 2 s base, 1 h cap), `Retry-After` is honoured up to a cap, and after the last try the job is `dead`.
- `/ui` needs `ADMIN_TOKEN`, as a bearer token or as the Basic auth password. Replay creates fresh jobs, it never edits history. The page is server-rendered with askama and a vendored htmx file, no CDN.
- `/healthz`, `/metrics` (Prometheus), and on SIGTERM the server stops accepting and lets in-flight deliveries finish. `prune --older-than 30d` deletes old deliveries but keeps anything still queued.

## Stack

Rust 1.99 (edition 2024), axum 0.8, tokio 1.54, sqlx 0.9 with Postgres 17, reqwest 0.13 (rustls), hmac 0.13 and sha2 0.11, askama 0.16 with htmx 2.0.11, metrics 0.24 with the Prometheus exporter, clap 4.6, wiremock 0.6 for tests.

## Run it

You need Docker. No Rust toolchain on your machine is required.

```bash
docker compose up --build
examples/send.sh github        # also: midtrans, xendit, partner, bad
```

Then open http://127.0.0.1:8020/ui and sign in with any user name and `demo-admin-token` as the password. The compose file uses demo secrets and publishes ports on `127.0.0.1` only. Put real values in `.env` (see `.env.example`).

The `partner` source points at an endpoint that always answers 503, so about 10 seconds after sending one it shows up in the dead-letter view. Retry settings in `inbox.toml` are shortened for that.

Or only Postgres in Docker and the service on your machine (this one needs Rust):

```bash
docker compose up -d db          # on 127.0.0.1:5432, DB_PORT moves it
cp .env.example .env             # fill in the secrets, openssl rand -hex 32
set -a; . ./.env; set +a         # the binary reads the environment, not the file
cargo run --bin echo-sink &      # demo targets on 127.0.0.1:8021, or point SINK_URL at your app
cargo run -- serve
cargo run -- prune --older-than 30d --dry-run
```

If you change `DB_PORT`, change the port in `DATABASE_URL` too.

| Variable | Default | |
| --- | --- | --- |
| `DATABASE_URL` | none | Postgres connection string |
| `INBOX_CONFIG` | `inbox.toml` | path to the config |
| `ADMIN_TOKEN` | none | protects `/ui`, which answers 503 while it is empty |
| `<secret_env>` | none | one per source, named in `inbox.toml`, required at start |
| `RUST_LOG` | `info` | log filter, add `tower_http=debug` for one access line per request |

On a machine without Rust, `scripts/cargo-docker.sh test` runs cargo in the official Rust image with cached volumes. It passes `TEST_DATABASE_URL` through only if you set it (use `host.docker.internal` for a Postgres on your machine), so the database tests are skipped otherwise.

## Tests

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
TEST_DATABASE_URL=postgresql://postgres:postgres@localhost:5432/webhook_inbox_test cargo test
```

There are 51 unit tests (every verifier with known vectors, tampered bodies, wrong secrets and stale timestamps, plus backoff, config parsing and event ids) and 41 integration tests. The integration tests use a Postgres schema per test and wiremock as the target, and cover forwarding, retry then success, dead after the last try, dedupe (including twenty racing requests with one event id), replay, prune, the UI and two worker pools competing for the same jobs. They are skipped without `TEST_DATABASE_URL`. CI runs format, clippy, tests and a release build with a Postgres service.

## Numbers

One laptop (Ryzen 9 5900HX, 16 GB), Docker Desktop, the compose stack with Postgres 17, 4 workers and a sink that answers 200 and logs nothing (`SINK_QUIET=1`). The load tool is `oha` 1.16.0 in a container on the same Docker network, 50 connections for 30 seconds, a small signed GitHub-style payload (141 bytes), every request stored. It shares the CPU with Postgres and the inbox, so a separate load machine would do better. Script: `scripts/bench.sh`.

| Run | Result |
| --- | --- |
| Receive, uncapped | 2339 requests/s, p50 19 ms, p95 38 ms, p99 61 ms, 70176 answered 200 and 50 were cut off at the 30 s mark (70197 stored, so 21 of those got in) |
| Drain after that run | 49320 queued jobs delivered in 38.8 s, about 1270 deliveries/s |
| End to end, capped at 800 requests/s for 20 s | 15998 answered 200 and 2 were cut off at the deadline, 15999 stored and delivered, queue empty 0.7 s after the last request |

Receiving is faster than delivering, so a burst builds a backlog and the workers catch up afterwards. At 800 requests/s they keep up. These are one run each, not averages. The raw `oha` output and run notes are in `docs/bench/`. I measured them at commit `699b6fe`, before the later changes to job claiming, tracing and the latency histogram, and I have not rerun them: the laptop was busy with other builds when I tried, so a new run would not be comparable.

## What verified means

| Source | The check covers | It does not cover |
| --- | --- | --- |
| GitHub, generic HMAC | the whole body | a replay, unless the HMAC source sets a timestamp tolerance (GitHub sends none) |
| Midtrans | `order_id`, `status_code`, `gross_amount` and the server key | `transaction_status`, `transaction_id`, `fraud_status` and the rest of the body, and there is no timestamp |
| Xendit | a shared token in `x-callback-token` | the body, which is not signed at all |

A Midtrans body with the same `signature_key` and a different `transaction_status` passes, because the status is not part of what gets hashed. The event id used for dedupe (`transaction_id` plus `transaction_status`) also comes from unsigned fields. For Xendit, whoever holds the token can send any body. The delivery page prints the same limits under Verification.

So treat a verified Midtrans or Xendit callback as "this probably came from the provider", not as proof of the status in it. Before your handler moves money or marks an order paid, ask the provider for the current state and act on that answer:

- Midtrans: `GET /v2/{order_id}/status` with your server key as the Basic auth user.
- Xendit: fetch the invoice (`GET /v2/invoices/{id}`) with your secret API key.
- `x-inbox-verified` on every forwarded request (`body-hmac`, `midtrans-signed-fields` or `shared-token`) tells your handler which sources need that call.

I wrote those two calls from the providers' docs and have not run them against live accounts. The inbox does not make them for you.

## Notes

- I've only run this against my own demo endpoints and made-up payloads. The Midtrans and Xendit verifiers are written against their docs and tested with computed vectors, not with live callbacks, and they cover less of the body than the HMAC ones.
- Verifier secrets live in env vars, one per source. There is no rotation and no second secret during a changeover.
- Admin auth is one shared token with no user accounts or roles. Put it behind TLS and do not expose it on the open internet.
- Bodies are stored as received, up to `max_body_bytes` (1 MB in the demo config). The UI shows them to anyone with the token, so personal data in a payload is visible there.
- Delivery is at least once. If a target answers 2xx and the inbox dies before it records that, the job is sent again, so your handler should be idempotent. The `x-inbox-delivery-id` header is there for that.
- Next: a per-target rate limit, and a signed header on forwarded requests.

## License

MIT
