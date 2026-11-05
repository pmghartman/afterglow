# Contributing

## Setup

```bash
bun install
```

## Before a pull request

```bash
bun run fmt
```

```bash
bun run check && bun run lint && bun test && bun run test:host && bun run test:e2e
```

`test:e2e` needs Xvfb, xterm and ffmpeg. There is no CI on pushes or pull requests: run these yourself. The one workflow, **release**, runs them again before publishing; to try a host change on Windows, run it with **Build only**.

## Where things go

- A change to the link's wire format touches both ends: `host/src/wire.rs` and `host/src/link.rs`, and `src/afterglow/link.ts`. Update `docs/protocol.md`, the tests in `host/src/wire.rs` and `host/tests/link.rs`, and `test/link.test.ts` with it.
- A change to the `Source` contract (`src/source.ts`) must keep drivers compiling; Phosphorus declares its own copy and its type check catches drift.
- A host change that touches a platform keeps the other one building: `cargo clippy --target x86_64-pc-windows-gnu` checks the Windows side from Linux (mingw-w64).

## Style

- TypeScript: Biome formats it (`bun run fmt`), two spaces, double quotes, 120 columns.
- Rust: `cargo fmt` (the host's `rustfmt.toml`), `cargo clippy` clean.
- Comments say why, not what.

## Licence of contributions

Contributions under `host/` are GPL-3.0-only; everything else is Apache-2.0.
