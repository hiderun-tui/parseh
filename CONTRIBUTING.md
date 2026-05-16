# Contributing

PARSEH is designed for people on restricted networks, where operational mistakes can have real human consequences. Contribute with that in mind: precision over hype, working code over promises.

## Pseudonymity

Contributors are never required to reveal a real-world identity. Use a pseudonym. Do not add personal identifying information to commits, code, docs, or issues.

## Build & test

```bash
cd server
cargo build --release --workspace
cargo test --workspace
```

`bash scripts/demo.sh` builds the core binaries and runs the offline 3-node acceptance test.

## Pull requests

- Keep changes focused. Explain the *why* in the description, not the code.
- Add or update tests for behavior changes.
- No overclaiming in code, comments, or docs — match what the code actually does. Keep open problems stated as open.
- Run `cargo fmt` and `cargo clippy` before submitting.

## Security

Never report a vulnerability in a public issue or PR. See [`SECURITY.md`](./SECURITY.md).

## License

By contributing you agree your contributions are licensed under [Apache 2.0](./LICENSE).
