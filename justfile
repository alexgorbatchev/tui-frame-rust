default:
    @just --list

build:
    cargo build --all-targets

# Run the interactive frame example
dev:
    cargo run --example frame

test:
    cargo test --all-targets

fmt:
    cargo fmt --package tui-frame

lint:
    cargo fmt --package tui-frame -- --check
    cargo clippy --all-targets -- -D warnings

coverage:
    cargo llvm-cov --all-targets --fail-under-lines 90

doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

check: lint test coverage doc
