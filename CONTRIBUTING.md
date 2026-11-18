# Contributing to Portagenty

Fork and clone the repository. Install [stable Rust](https://rustup.rs/) with
`rustfmt` and `clippy` (the checked-in toolchain file lists both), and install
[cargo-nextest](https://nexte.st/docs/installation/). For end-to-end tests,
install [tmux](https://github.com/tmux/tmux/wiki/Installing) and, on Linux,
[Zellij](https://zellij.dev/documentation/installation). Put both on `PATH`.

From the repository root, run the formatting, linting, and test commands used
in [CI](./.github/workflows/ci.yml):

```sh
cargo fmt --all -- --check && \
  RUSTFLAGS='-D warnings' cargo clippy --all-targets --all-features -- -D warnings && \
  RUSTFLAGS='-D warnings' cargo nextest run --no-fail-fast && \
  RUSTFLAGS='-D warnings' cargo nextest run --no-fail-fast --features tmux-e2e && \
  if [ "$(uname -s)" = Linux ]; then
    RUSTFLAGS='-D warnings' cargo nextest run --no-fail-fast --features zellij-e2e --test mux_zellij_e2e
  fi
```

GitHub Actions runs these checks and a locked release build on hosted Linux and
macOS runners. The Zellij suite runs only on Linux, matching CI.
