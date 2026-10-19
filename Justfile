check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

workflows-check:
    just --justfile "{{env_var('ACTIONS_PATH')}}/lint/Justfile" --working-directory . check

release-build:
    test "$GITHUB_REF_NAME" = "v$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')"
    cargo build --locked --release --target x86_64-unknown-linux-musl

release-publish:
    gh release create "$GITHUB_REF_NAME" target/x86_64-unknown-linux-musl/release/cgwalters-forge-misctool --verify-tag --generate-notes --repo "$GITHUB_REPOSITORY"
