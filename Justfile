# Justfile for Plank

run:
    cargo run --release

build:
    cargo build --release

fmt:
    cargo fmt

lint:
    cargo clippy -- -D warnings
