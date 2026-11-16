# syntax=docker/dockerfile:1
# Multi-stage build: compile on the Rust image, run on distroless as a non-root user.
FROM rust:1.100-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p energybrain \
 && install -D target/release/energybrain /out/energybrain \
 && mkdir -p /out/data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/energybrain /usr/local/bin/energybrain
# /data holds the SQLite database; it must be writable by the nonroot user (65532).
COPY --from=build --chown=65532:65532 /out/data /data
ENV ENERGYBRAIN_CONFIG=/config/energybrain.toml \
    ENERGYBRAIN_LOG_FORMAT=json
VOLUME ["/data"]
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/energybrain"]
