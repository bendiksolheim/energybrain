set shell := ["bash", "-euo", "pipefail", "-c"]

default: check

# fmt check, clippy, tests and the 100 % line-coverage gate
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features
    just cov

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

# energybrain against the simulated house, on accelerated time (docs/dev.md)
dev config="config/dev.toml" sim="config/sim.toml": dev-broker
    rm -f target/dev/energybrain.db target/dev/energybrain.db-*
    cargo run -p energybrain-sim -- {{config}} {{sim}}

# (Re)start the dev broker: mosquitto in an Apple container, empty every time
dev-broker:
    container system start
    -container stop energybrain-mqtt
    -container delete energybrain-mqtt
    container run -d --name energybrain-mqtt -p 127.0.0.1:1883:1883 -v "{{justfile_directory()}}/deploy/dev/mosquitto:/mosquitto/config" eclipse-mosquitto:2

# Stop and remove the dev broker
dev-down:
    -container stop energybrain-mqtt
    -container delete energybrain-mqtt

# Change the simulated house live: `just sim ev 7.4`, `just sim meter pause`, `just sim zone/bad/setpoint 28`
sim control value:
    container exec energybrain-mqtt mosquitto_pub -t "sim/{{control}}/set" -m "{{value}}"

# Watch topics on the dev broker (default: everything energybrain publishes)
dev-sub topic="energybrain/#":
    container exec energybrain-mqtt mosquitto_sub -v -t "{{topic}}"

# Publish any message on the dev broker: `just dev-pub energybrain/mode/set observe`
dev-pub topic value:
    container exec energybrain-mqtt mosquitto_pub -t "{{topic}}" -m "{{value}}"
