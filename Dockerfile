FROM rust:1.97-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97 AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY templates ./templates
COPY static ./static
RUN cargo build --locked --release

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home router \
    && mkdir -p /data && chown router:router /data
COPY --from=build /build/target/release/shared-router /usr/local/bin/shared-router
USER 10001:10001
WORKDIR /data
ENV BIND_ADDRESS=0.0.0.0:8080 DATABASE_URL=sqlite:///data/router.sqlite
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s CMD curl --fail --silent http://127.0.0.1:8080/readyz || exit 1
ENTRYPOINT ["shared-router"]
CMD ["serve"]
