# syntax=docker/dockerfile:1
# Multi-stage build: compile on the Rust image, run on distroless as a non-root user.
FROM rust:1.100-bookworm AS build
# cargo-leptos builds the server and the web app's browser bundle (PLAN.md §10). Pinned and
# checksum-verified; it downloads the matching wasm-bindgen and wasm-opt itself.
ARG CARGO_LEPTOS=0.3.11
ARG CARGO_LEPTOS_SHA256=1c9cf0e1e3ffae6c8b58d29281d949dab560cea613b80d361f323f93071df7c8
RUN curl -sSfLo /tmp/cargo-leptos.tar.gz \
      https://github.com/leptos-rs/cargo-leptos/releases/download/v${CARGO_LEPTOS}/cargo-leptos-x86_64-unknown-linux-gnu.tar.gz \
 && echo "${CARGO_LEPTOS_SHA256}  /tmp/cargo-leptos.tar.gz" | sha256sum -c - \
 && tar xzf /tmp/cargo-leptos.tar.gz -C /usr/local/cargo/bin --strip-components=1 \
      cargo-leptos-x86_64-unknown-linux-gnu/cargo-leptos \
 && rm /tmp/cargo-leptos.tar.gz
WORKDIR /src
COPY . .
# `cargo fetch --locked` fails if Cargo.lock is out of date (cargo-leptos has no --locked).
# `--build-arg JOBS=3` caps parallel compile jobs on machines short of memory.
ARG JOBS=
RUN if [ -n "$JOBS" ]; then export CARGO_BUILD_JOBS="$JOBS"; fi \
 && cargo fetch --locked \
 && cargo leptos build --release \
 && install -D target/release/energybrain /out/energybrain \
 && cp -r target/site /out/site \
 && mkdir -p /out/data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/energybrain /usr/local/bin/energybrain
COPY --from=build /out/site /site
# /data holds the SQLite database; it must be writable by the nonroot user (65532).
COPY --from=build --chown=65532:65532 /out/data /data
ENV ENERGYBRAIN_CONFIG=/config/energybrain.toml \
    ENERGYBRAIN_SITE_ROOT=/site \
    ENERGYBRAIN_LOG_FORMAT=json
VOLUME ["/data"]
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/energybrain"]
