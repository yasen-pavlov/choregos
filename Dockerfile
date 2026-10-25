# syntax=docker/dockerfile:1
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev build-base cmake perl
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY choregos.example.toml askama.toml ./
RUN cargo build --release --locked && cp target/release/choregos /choregos

FROM gcr.io/distroless/static:nonroot
COPY --from=build /choregos /usr/local/bin/choregos
USER 65532:65532
EXPOSE 8095
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 CMD ["/usr/local/bin/choregos", "healthz", "--url", "http://127.0.0.1:8095/healthz"]
ENTRYPOINT ["/usr/local/bin/choregos"]
CMD ["run", "--config", "/config/choregos.toml"]
