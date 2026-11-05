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
bun run check && bun run lint && bun test && bun run test:wire
```

There is no CI on pushes or pull requests: run these yourself. The one workflow, **release**, runs them again before publishing; to try a host change on Windows, run it with **Build only**.

## Where things go

- A change to the link's wire format touches both ends: `host/src/phos_wire.h` and `host/src/phos_link.cpp`, and `src/afterglow/link.ts`. Update `docs/protocol.md`, `host/test/wire_test.cpp` and `test/link.test.ts` with it.
- A change to the `Source` contract (`src/source.ts`) must keep drivers compiling; Phosphorus declares its own copy and its type check catches drift.
- Keep host changes small and marked: the less of Apollo's tree that differs, the easier its fixes are to take.

## Style

- TypeScript: Biome formats it (`bun run fmt`), two spaces, double quotes, 120 columns.
- C++: the host's `.clang-format`, as Apollo has it.
- Comments say why, not what.

## Licence of contributions

Contributions under `host/` are GPL-3.0-only; everything else is Apache-2.0.
