host := `rustc -vV | sed -n 's/^host: //p'`

# Build the plugin component: target/wasm32-wasip2/release/pumpkin_skinrestorer.wasm
build:
    cargo build --release

# Unit tests for the pure modules run natively (the plugin glue is wasm-only).
test:
    cargo test --target {{host}}

lint:
    cargo fmt --check
    cargo clippy --release -- -D warnings
    cargo clippy --target {{host}} --tests -- -D warnings

# Compare wit/ with Pumpkin's plugin WIT at the commit named in wit/PUMPKIN_COMMIT.
wit-check:
    #!/usr/bin/env bash
    set -euo pipefail
    commit=$(cat wit/PUMPKIN_COMMIT)
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    curl -fsSL "https://github.com/Pumpkin-MC/Pumpkin/archive/$commit.tar.gz" \
        | tar -xz -C "$tmp" --strip-components=1
    diff -r --exclude=PUMPKIN_COMMIT --exclude='LICENSE*' \
        "$tmp/crates/pumpkin-plugin-wit/v0.2" wit
    echo "wit/ matches Pumpkin $commit"

# Show what the component imports and exports.
wit: build
    wasm-tools component wit target/wasm32-wasip2/release/pumpkin_skinrestorer.wasm
