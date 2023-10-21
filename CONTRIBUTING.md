# Contributing to Compio

Thanks for your help improving the project! We are so happy to have you! :tada:

There are opportunities to contribute to Compio at any level. It doesn't matter if
you are just getting started with Rust or are the most weathered expert, we can
use your help. If you have any question about Compio, feel free to join [our group](https://t.me/compio_rs) in telegram.

This guide will walk you through the process of contributing to Compio on following topics:

- [General guidelines](#general-guidelines)
  - [Develop Guide](#develop-guide)
  - [Style Guide](#style-guide)
- [Contribute with issue](#contribute-with-issue)
- [Contribute with pull request](#contribute-with-pull-request)

## General guidelines

We adhere to [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct). tl;dr: **be nice**. Before making any contribution, check existing issue and pull requests to avoid duplication of effort. Also, in case of bug, try updating to the latest version of Compio and/or rust might help.

### Develop Guide

- Use nightly toolchain to develop and run `rustup update` regularly. Compio does use nightly features, behind feature gate; so, when testing with `--all-features` flag, only nightly toolchain would work.

### Style Guide

- Use `cargo fmt --all` with nightly toolchain to format your code (for nightly `rustfmt` features, see detail in [`rustfmt.toml`]).
- Use `cargo clippy --all` to check any style/code problem.
- Use [Angular Convention](https://github.com/angular/angular/blob/main/CONTRIBUTING.md#-commit-message-format) when making commits
- When adding new crate, add `#![warn(missing_docs)]` at the top of `lib.rs`

[`rustfmt.toml`]: https://github.com/compio-rs/compio/blob/master/rustfmt.toml

## Contribute with issue

If you find a bug or have a feature request, please [open an issue](https://github.com/compio-rs/compio/issues/new/choose) with detailed description. Issues that are lack of informaton or destructive will be requested for more information or closed.

It's also helpful if you can provide the following information:

- A minimal reproducible example
- The version of Compio you are using.
- The version of Rust you are using.
- Your environment (OS, Platform, etc).

## Contribute with pull request

We welcome any code contributions. It's always welcome and recommended to open an issue to discuss on major changes before opening a PR. And pull requests should:

- follow the [Style Guide](#style-guide).
- pass CI tests and style check. You can run `cargo test --all-features` locally to test, but result may differ from CI depend on local environment -- it might be a good chance to contribute!
- be reviewed by at least one maintainer before getting merged.
- have a description of what it does and why it is needed in PR body.
