# Contributing to Beyrix

Thanks for helping make Beyrix a better terminal. We welcome bug reports, compatibility findings, documentation, tests, translations, packaging work, and code contributions.

## Before you start

1. Search existing [issues](https://github.com/Beykus-Y/Beyrix/issues) and pull requests to avoid duplicate work.
2. For a small, clear fix, submit a focused PR. For architecture changes or large features, open an issue first and discuss the approach.
3. For a first contribution, look for `good first issue` and `help wanted` labels once the maintainers have assigned them.
4. Be respectful and follow our [Code of Conduct](CODE_OF_CONDUCT.md).

## Local setup

Beyrix is currently a Linux/Wayland Rust project (edition 2024). Read the [architecture overview](docs/ARCHITECTURE.md) for an introduction to the existing modules. You will need a recent stable Rust toolchain, Bash, the Wayland development libraries, and other native dependencies provided by your Linux distribution.

```sh
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

The graphical emulator needs an active Wayland session; tests and command-line help do not require a visible window. Some tests exercise PTYs, Bash and external programs; provide logs if they fail on your environment.

## Good pull requests

- Describe the problem, change, and testing, plus related issues.
- Include a minimal reproducer for regressions. For terminal escape sequences, include exact input bytes and expected behavior if possible.
- Keep PRs narrowly scoped and preserve existing keyboard, selection, clipboard, and shell behavior.
- Add or update automated tests for behavior changes; explain omissions.
- Avoid unrelated formatting, dependency updates or architectural refactors.
- Clearly distinguish measured benchmark results from estimates, including hardware, configuration and methodology.
- Don't paste sensitive logs, tokens, private environment variables, or personal paths.

## Licensing contributions

The project uses **MPL-2.0** (see [LICENSE](LICENSE)). By contributing, you confirm you are permitted to submit the work under MPL-2.0 and have the rights to any code or assets you include. Do not copy third-party code or images without verifying license compatibility and required attribution.

We do **not** require a contributor license agreement (CLA) or copyright assignment. Contributors retain their copyrights. Pull requests and reviews do not automatically guarantee acceptance.

## Review process

Maintainers may request tests, a reproducer, design changes or smaller commits. Decisions should prioritize correctness, security, portability, usability and maintainability. There is no guaranteed review turnaround time, but constructive follow-up is welcome.

Windows and GPU backend work is welcome as design proposals; do not assume that platform APIs or rendering interfaces already exist.
