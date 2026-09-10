# syntax=docker/dockerfile:1
FROM rust:1.96-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    CARGO_BUILD_JOBS=2 cargo build --locked --release --features server \
    && install -m 0755 target/release/starfield-datastore /usr/local/bin/starfield-datastore

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 starfield \
    && useradd --uid 10001 --gid starfield --home-dir /var/lib/starfield-datastore starfield \
    && install -d -o starfield -g starfield /var/lib/starfield-datastore/cache
COPY --from=build /usr/local/bin/starfield-datastore /usr/local/bin/starfield-datastore
COPY manifests/ephemeris.toml /usr/share/starfield-datastore/ephemeris.toml
LABEL org.opencontainers.image.source="https://github.com/OrbitalCommons/starfield-datastore" \
      org.opencontainers.image.description="Validated artifact cache and private ephemeris server" \
      org.opencontainers.image.licenses="MIT"
ENV STARFIELD_CACHE_DIR=/var/lib/starfield-datastore/cache
WORKDIR /var/lib/starfield-datastore
USER 10001:10001
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/starfield-datastore"]
CMD ["--config", "/etc/starfield-datastore/config.toml", "--service", "ephemeris", "serve"]
