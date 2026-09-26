FROM rust:1.97-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS build
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

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 router \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin router \
    && install -d -o 10001 -g 10001 -m 0700 /data
COPY --from=build /shared-router /usr/local/bin/shared-router
USER 10001:10001
WORKDIR /data
ENV BIND_ADDRESS=0.0.0.0:8080 DATABASE_URL=sqlite:///data/router.sqlite
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s CMD curl --fail --silent http://127.0.0.1:8080/readyz || exit 1
ARG REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/SobshDev/shared_router" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.revision="$REVISION"
ENTRYPOINT ["shared-router"]
CMD ["serve"]
