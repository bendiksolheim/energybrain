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
