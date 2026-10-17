# Contributing

Thanks for helping with spew mesh. This page covers the rules, the checks, and how a release is made.

## Write it yourself

spew mesh is a clean-room implementation, written from its own design document, [docs/design.md](docs/design.md).
Keep it that way:

- Work from the design document, the public documentation of the interfaces we use (iroh, MCP, Claude Code,
  Codex, OpenCode), and the docs of the crates in `Cargo.toml`.
- Don't paste or translate code from other projects that connect agents to each other, and don't read their source
  while you work on the same part here.
- When the design is unclear or wrong, open an issue or change `docs/design.md` in the same pull request, and say
  why. The design document stays the contract: code and docs follow it.

## The repo is public

- No secrets, tokens, keys or real environment values, anywhere: code, tests, fixtures, comments, commit messages.
- No real people, email addresses, host names or paths from your computer. Examples use made-up people (`sam`,
  `maya`, `lee`), devices like `sam-mac` and `maya-mbp`, and hosts under `example.com`.
- CI scans every commit for secrets with gitleaks.

## Checks

Every pull request must pass:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs them on Linux and macOS with stable Rust, and runs `cargo check` with Rust 1.91, the oldest supported
version (`rust-version` in `Cargo.toml`). Don't raise it without a reason.

The OpenCode plugin tests need `node` on `PATH`; without it they say they were skipped.

## Tests

Tests never touch the network, your home folder, or your real agent configs:

- Each test sets `SPEW_MESH_DIR` to a temporary folder and `SPEW_MESH_LAN=0`, and points `HOME`,
  `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `XDG_CONFIG_HOME` at temporary folders when it needs them.
- Tests never run the real `claude`, `codex` or `opencode`. They use small fake scripts written to a temporary
  folder that record their arguments and print canned output.
- Daemons in integration tests use a roster file and `[network] mode = "local"` on `127.0.0.1`. The spew API is a
  mock server inside the test.

## Code

- Rust edition 2024. Add a dependency only when you truly need it, and pin it like the others.
- Fail closed: a bad setting, frame or value is an error, never a silent default that widens access.
- No `unwrap` or `expect` on anything that can fail at runtime outside tests. Errors say what failed.
- Comments are sparse and say why. Section numbers in comments point into `docs/design.md`.
- Unsafe code is denied crate-wide. A module that needs a libc call allows it locally, with a `SAFETY` comment.

## Docs

User-facing changes update the docs in the same pull request: the README, the page under `docs/` that covers the
feature, and `CHANGELOG.md` under the unreleased version.

## Releases

For the maintainers:

1. Update `version` in `Cargo.toml` and run `cargo build` so `Cargo.lock` follows. Move the `CHANGELOG.md` entries
   under the new version with today's date. Merge that to `main`.
2. Tag the merge commit and push the tag:

   ```sh
   git tag -a v0.1.0 -m "spew mesh 0.1.0"
   git push origin v0.1.0
   ```

   The release workflow checks that the tag matches `Cargo.toml`, builds `spew` for Linux (x86_64 and arm64, static
   musl) and macOS (arm64 and x86_64), and attaches the archives and `SHA256SUMS` to a GitHub release for the tag.
3. Publish to crates.io by hand, from a clean checkout of the tag, with the owner's crates.io token. CI holds no
   crates.io token.

   ```sh
   cargo login                      # once, with the owner's token
   cargo publish --locked --dry-run
   cargo publish --locked
   ```

## License

By contributing, you agree that your contribution is dual licensed under the [MIT license](LICENSE-MIT) and the
[Apache License, Version 2.0](LICENSE-APACHE), as the rest of the project is, without any additional terms or
conditions.
