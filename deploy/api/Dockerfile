# pair-api image. Build from the repo root:
#   docker build -f deploy/api/Dockerfile -t pair-api:<immutable-tag> .
# Migrations are mounted at /app/migrations by compose (read-only), not baked in.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY service/ ./service/
RUN cargo build --release --locked --manifest-path service/Cargo.toml -p pair-api --bin pair-api

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home /var/lib/pair pair \
 && mkdir -p /var/lib/pair /app/migrations && chown pair /var/lib/pair
COPY --from=build /src/service/target/release/pair-api /usr/local/bin/pair-api
USER pair
WORKDIR /app
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/pair-api"]
