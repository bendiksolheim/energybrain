set shell := ["bash", "-euo", "pipefail", "-c"]

# Container runtime for the dev broker: Apple `container` on macOS, Docker elsewhere.
# Override with `EB_RUNTIME=docker just dev` (e.g. Docker Desktop or OrbStack on a Mac).
runtime := env("EB_RUNTIME", if os() == "macos" { "container" } else { "docker" })
runtime_rm := if runtime == "container" { "delete" } else if runtime == "docker" { "rm" } else { error("EB_RUNTIME must be `container` or `docker`") }

default: check

# fmt check, clippy, tests and the 100 % line-coverage gate
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features
    just cov
    just wasm-check

# The crates the browser runs must keep compiling for wasm32 (PLAN.md §10)
wasm-check:
    cargo check --target wasm32-unknown-unknown -p energybrain-view

fmt:
    cargo fmt --all

test:
    cargo test --workspace --all-features

cov:
    cargo llvm-cov --workspace --all-features --fail-under-lines 100 --ignore-filename-regex '(^|/)main\.rs$'

# HTML coverage report in target/llvm-cov/html
cov-html:
    cargo llvm-cov --workspace --all-features --ignore-filename-regex '(^|/)main\.rs$' --html

run config="config/example.toml":
    cargo run -p energybrain -- {{config}}

docker:
    docker build -f deploy/Dockerfile -t energybrain .

# Architecture diagrams (docs/architecture): render, then show with kitty graphics, in a new Rex
# tab when inside Rex. `just arch engine` shows one view.
arch *views="system engine crates": arch-render
    #!/usr/bin/env bash
    set -euo pipefail
    pngs=(); for v in {{views}}; do pngs+=("target/arch/$v.png"); done
    if [ -n "${REX_SESSION:-}" ]; then
      rex window new -s "$REX_SESSION" Architecture -c "{{justfile_directory()}}" --keep-open -- scripts/kitty-show.sh "${pngs[@]}" >/dev/null
    else
      scripts/kitty-show.sh "${pngs[@]}"
    fi

# Regenerate the crate view from `cargo metadata`; render every view to SVG (committed) and PNG
arch-render:
    scripts/arch-crates.sh > docs/architecture/crates.d2
    mkdir -p target/arch
    for v in system engine crates; do \
      d2 --layout=elk --dark-theme=200 docs/architecture/$v.d2 docs/architecture/$v.svg 2>&1 | grep -v '^success' || true; \
      d2 --layout=elk docs/architecture/$v.d2 target/arch/$v.png 2>&1 | grep -v '^success' || true; \
    done

# Live browser preview while editing a view: `just arch-edit engine`
arch-edit view="system":
    d2 --watch --layout=elk docs/architecture/{{view}}.d2 target/arch/{{view}}.svg

# energybrain against the simulated house, on accelerated time (docs/dev.md)
dev config="config/dev.toml" sim="config/sim.toml": dev-broker
    rm -f target/dev/energybrain.db target/dev/energybrain.db-*
    cargo run -p energybrain-sim -- {{config}} {{sim}}

# (Re)start the dev broker: mosquitto in a container, empty every time
dev-broker: _runtime-start dev-down
    {{runtime}} run -d --name energybrain-mqtt -p 127.0.0.1:1884:1883 -v "{{justfile_directory()}}/deploy/dev/mosquitto:/mosquitto/config" eclipse-mosquitto:2

# Stop and remove the dev broker
dev-down:
    -{{runtime}} stop energybrain-mqtt
    -{{runtime}} {{runtime_rm}} energybrain-mqtt

# Change the simulated house live: `just sim ev 7.4`, `just sim meter pause`, `just sim zone/bathroom/setpoint 28`
sim control value:
    {{runtime}} exec energybrain-mqtt mosquitto_pub -t "sim/{{control}}/set" -m "{{value}}"

# Watch topics on the dev broker (default: everything energybrain publishes)
dev-sub topic="energybrain/#":
    {{runtime}} exec energybrain-mqtt mosquitto_sub -v -t "{{topic}}"

# Publish any message on the dev broker: `just dev-pub energybrain/mode/set observe`
dev-pub topic value:
    {{runtime}} exec energybrain-mqtt mosquitto_pub -t "{{topic}}" -m "{{value}}"

# Apple `container` needs its system service running; Docker's daemon is already up
[private]
_runtime-start:
    {{ if runtime == "container" { "container system start" } else { "" } }}
