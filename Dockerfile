# CEngine server image: one `ce-serve` binary, configured by environment variables.
# Build:  docker build -t cengine .
# Run:    docker run -p 8080:8080 -e CE_DATABASE_URL=... -e CE_MANIFEST=/manifest.json \
#           -e CE_JWT_KEY=... -e CE_INSTALLATION=demo -v ./manifest.json:/manifest.json:ro cengine
# First run against an empty database: add the argument `install` once to create the schema.
# Liveness and readiness: GET /_meta/health/live and /_meta/health/ready.

FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY spec/manifest.schema.json ./spec/manifest.schema.json
RUN cargo build --release --locked -p ce-serve

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 render \
    && useradd --system --no-create-home --uid 10001 --groups render ce
COPY --from=build /src/target/release/ce-serve /usr/local/bin/ce-serve
USER ce
ENV CE_BIND=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["ce-serve"]
