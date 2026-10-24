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
- Verifiers: GitHub (`X-Hub-Signature-256`), generic HMAC-SHA256 with a configurable header and an optional Stripe-style `t=...,v1=...` timestamp tolerance, Midtrans (sha512 of `order_id + status_code + gross_amount + server_key` against `signature_key` in the body) and Xendit (`x-callback-token`). Every comparison goes through `subtle`, so it is constant time.
- A verified request has its raw body, headers and result stored, plus one job per target, and the answer goes out before any forwarding. A second delivery with the same event id gets `200` with `x-inbox-duplicate: true` and no new jobs. Credentials such as `authorization` and `x-callback-token` are not stored.
- Workers claim jobs with `SELECT ... FOR UPDATE SKIP LOCKED` and a lease, so several instances can share one database. Each attempt POSTs the original body with `x-inbox-delivery-id` and `x-inbox-attempt`. A 2xx ends the job. Anything else retries with exponential backoff and full jitter (defaults: 8 tries, 2 s base, 1 h cap), `Retry-After` is honoured up to a cap, and after the last try the job is `dead`.
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

The `partner` source points at an endpoint that always answers 503, so about a minute after sending one it shows up in the dead-letter view. Retry settings in `inbox.toml` are shortened for that.

Or only Postgres in Docker and the service on your machine:

```bash
cp .env.example .env     # fill in the secrets, openssl rand -hex 32
cargo run -- serve
cargo run -- prune --older-than 30d --dry-run
```

| Variable | Default | |
| --- | --- | --- |
| `DATABASE_URL` | none | Postgres connection string |
| `INBOX_CONFIG` | `inbox.toml` | path to the config |
| `ADMIN_TOKEN` | none | protects `/ui`, which answers 503 while it is empty |
| `<secret_env>` | none | one per source, named in `inbox.toml`, required at start |
| `RUST_LOG` | `info` | log filter |

On a machine without Rust, `scripts/cargo-docker.sh test` runs cargo in the official Rust image with cached volumes.

## Tests

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
TEST_DATABASE_URL=postgresql://postgres:postgres@localhost:5432/webhook_inbox_test cargo test
```

There are 47 unit tests (every verifier with known vectors, tampered bodies, wrong secrets and stale timestamps, plus backoff, config parsing and event ids) and 38 integration tests. The integration tests use a Postgres schema per test and wiremock as the target, and cover forwarding, retry then success, dead after the last try, dedupe, replay, the UI and two worker pools competing for the same jobs. They are skipped without `TEST_DATABASE_URL`. CI runs format, clippy, tests and a release build with a Postgres service.

## Numbers

One laptop (Ryzen 9 5900HX, 16 GB), Docker Desktop, the compose stack with Postgres 17, 4 workers and a sink that answers 200 and logs nothing (`SINK_QUIET=1`). The load tool is `oha` 1.16.0 in a container on the same Docker network, 50 connections for 30 seconds, a small signed GitHub-style payload (141 bytes), every request stored. It shares the CPU with Postgres and the inbox, so a separate load machine would do better. Script: `scripts/bench.sh`.

| Run | Result |
| --- | --- |
| Receive, uncapped | 2339 requests/s, p50 19 ms, p95 38 ms, p99 61 ms, 70176 of 70176 answered 200 |
| Drain after that run | 49320 queued jobs delivered in 38.8 s, about 1270 deliveries/s |
| End to end, capped at 800 requests/s for 20 s | 15999 stored and 15999 delivered, queue empty 0.7 s after the last request |

Receiving is faster than delivering, so a burst builds a backlog and the workers catch up afterwards. At 800 requests/s they keep up. These are one run each, not averages. Raw output is in my notes folder, not in the repo.

## Notes

- I've only run this against my own demo endpoints and made-up payloads. The Midtrans and Xendit verifiers are written against their docs and tested with computed vectors, not with live callbacks.
- Verifier secrets live in env vars, one per source. There is no rotation and no second secret during a changeover.
- Admin auth is one shared token with no user accounts or roles. Put it behind TLS and do not expose it on the open internet.
- Bodies are stored as received, up to `max_body_bytes` (1 MB in the demo config). The UI shows them to anyone with the token, so personal data in a payload is visible there.
- Delivery is at least once. If a target answers 2xx and the inbox dies before it records that, the job is sent again, so your handler should be idempotent. The `x-inbox-delivery-id` header is there for that.
- Next: a per-target rate limit, and a signed header on forwarded requests.

## License

MIT
