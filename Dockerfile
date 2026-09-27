FROM rust:1.98-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
COPY static ./static
# Cache mounts keep compiled dependencies between builds, so source edits rebuild only this crate.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --locked --release \
    && cp target/release/shared-router /shared-router

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 router \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin router \
    && install -d -o 10001 -g 10001 -m 0700 /data
# The image distributes the binary, so it carries the license and third-party notices too.
COPY LICENSE THIRD_PARTY_NOTICES.md THIRD_PARTY_NOTICES_CRATES.md /usr/share/doc/shared-router/
COPY --from=build /shared-router /usr/local/bin/shared-router
USER 10001:10001
WORKDIR /data
ENV BIND_ADDRESS=0.0.0.0:8080 DATABASE_URL=sqlite:///data/router.sqlite
EXPOSE 8080
# The binary probes its own /readyz (5-second request timeout), so the image needs no curl.
HEALTHCHECK --interval=30s --timeout=6s --start-period=10s CMD ["shared-router", "healthcheck"]
ARG REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/SobshDev/shared_router" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.revision="$REVISION"
ENTRYPOINT ["shared-router"]
CMD ["serve"]
