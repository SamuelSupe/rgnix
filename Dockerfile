FROM rust:1.98-bookworm AS build
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends cmake pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=secret,id=build_ca \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=rgnix-target-${TARGETARCH},target=/src/target \
    if [ -f /run/secrets/build_ca ]; then export CARGO_HTTP_CAINFO=/run/secrets/build_ca; fi; \
    cargo clean --release -p rgnix && cargo build --release --locked -j 2 \
    && cp /src/target/release/rgnix /rgnix

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10101 --no-create-home rgnix
COPY --from=build /rgnix /usr/local/bin/rgnix
USER 10101:10101
EXPOSE 8080 8443 9090
STOPSIGNAL SIGTERM
ENTRYPOINT ["rgnix"]
