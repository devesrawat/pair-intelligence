# pair-api image. Build from the repo root:
#   docker build -f deploy/api/Dockerfile -t pair-api:<immutable-tag> .
# Migrations and the provider registry are baked in so an image tag is self-consistent
# (a rollback to an older tag brings its own migrations and models.yaml).
#
# Base images are pinned to version tags. At release, pin digests as well:
#   docker buildx imagetools inspect rust:1.93-bookworm   (then FROM rust:1.93-bookworm@sha256:...)
FROM rust:1.93-bookworm AS build
WORKDIR /src
COPY service/ ./service/
RUN cargo build --release --locked --manifest-path service/Cargo.toml -p pair-api --bin pair-api

FROM debian:12.13-slim
RUN useradd --system --uid 10001 --home /var/lib/pair pair \
 && mkdir -p /var/lib/pair /app/migrations /app/config && chown pair /var/lib/pair
COPY --from=build /src/service/target/release/pair-api /usr/local/bin/pair-api
COPY migrations/ /app/migrations/
COPY config/models.yaml /app/config/models.yaml
USER pair
WORKDIR /app
ENV PAIR_MIGRATIONS_DIR=/app/migrations \
    PAIR_MODELS_CONFIG=/app/config/models.yaml
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/pair-api"]
