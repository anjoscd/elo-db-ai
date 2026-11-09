# Contributing to ELO

Thanks for your interest. ELO is in its design stage, so the most useful contributions right now are critiques of the [spec](docs/SPEC.md) and the [evaluation method](docs/EVALUATION.md).

## Ground rules

- **Clean room.** Do not copy code from other context engines or memory tools whose licenses are not compatible with Apache-2.0. Read their docs and papers freely; write the code yourself.
- **Measured changes.** A pull request that changes selection, scoring, memory or the code map must include the `elo bench` delta (before → after) on the public fixture.
- **No private data.** Never commit real rules, memories, prompts, logs or code from your own projects. Fixtures are synthetic.
- **Local-first.** No telemetry, no network calls in the default build.
- **Small PRs.** One concern per pull request.

## Development (once code lands)

```bash
cargo build
cargo test            # must pass with networking disabled
cargo run -- bench    # evaluation harness
```

## Commit style

`type(scope): summary`, for example `feat(selector): layer quotas` or `fix(map): renders edge for namespaced JSX`.
