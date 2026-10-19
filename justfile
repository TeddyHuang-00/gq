_default:
    @just --choose

[doc("Format all code and sort Cargo.toml files")]
format:
    cargo +nightly fmt --all
    cargo sort > /dev/null 2>&1
    cargo sort-derives
    just --fmt --unstable

[doc("Run all checks including type checking, linting, and typo checking")]
check: format
    typos **/*.rs
    cargo machete
    cargo check --locked --all --quiet
    cargo clippy --locked --quiet --all-targets -- -D warnings

[doc("Fix lint warnings automatically (safely)")]
fix: format
    code=0; cargo machete --fix --with-metadata || code=$?; case $code in 0|1) exit 0;; *) exit 1;; esac
    cargo clippy --locked --fix --allow-dirty --allow-staged --quiet

[doc("Run all tests")]
test: check
    cargo test --locked

[doc("Record the demo GIF from a fresh release build")]
demo:
    cargo build --release --locked --quiet
    vhs assets/demo.tape
