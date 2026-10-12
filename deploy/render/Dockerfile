# CEngine STAGING image for the plugin phase (the Blueprint starts on the root Dockerfile). Same build as the root Dockerfile, plus the files a staging
# run needs baked in at fixed paths (Render free services have no persistent disk, so nothing is mounted):
#   /opt/ce/manifest.json          the manifest served (the repo's inventory example for now)
#   /opt/ce/plugins/               CE_PLUGIN_ROOT: holds .cengine/plugins/<id>/plugin.toml (pinned bytes)
#   /opt/ce/install-record/        holds installs.json, the approval record written by `ce-serve plugin approve`
# Build context is the repository root (dockerContext: .). See deploy/render/STAGING.md.

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
COPY spec/examples/inventory.json /opt/ce/manifest.json
COPY deploy/render/plugins /opt/ce/plugins
COPY deploy/render/install-record /opt/ce/install-record
USER ce
ENV CE_BIND=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["ce-serve"]
