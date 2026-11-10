host := `rustc -vV | sed -n 's/^host: //p'`

# Build the plugin component: target/wasm32-wasip2/release/pumpkin_skinrestorer.wasm
build:
    cargo build --release

# Unit tests for the pure modules run natively (the plugin glue is wasm-only).
test:
    cargo test --target {{host}}

lint:
    cargo fmt --check
    cargo clippy --release
    cargo clippy --target {{host}} --tests

# Show what the component imports and exports.
wit: build
    wasm-tools component wit target/wasm32-wasip2/release/pumpkin_skinrestorer.wasm
