FROM rust:1.99-bookworm AS build
WORKDIR /app
ENV CARGO_BUILD_JOBS=6

# Dependencies first so code changes don't rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && echo 'fn main() {}' > src/bin/echo-sink.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY migrations migrations
COPY templates templates
COPY static static
COPY src src
RUN touch src/main.rs src/lib.rs src/bin/echo-sink.rs \
 && cargo build --release --locked

FROM debian:bookworm-slim
# CA certificates for https targets.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --no-create-home --uid 10001 app
COPY --from=build /app/target/release/webhook-inbox /app/target/release/echo-sink /usr/local/bin/
USER app
EXPOSE 8020
HEALTHCHECK --interval=10s --timeout=4s --start-period=10s --retries=3 \
  CMD ["webhook-inbox", "healthcheck"]
ENTRYPOINT ["webhook-inbox"]
CMD ["serve"]
