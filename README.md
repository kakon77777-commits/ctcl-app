# CTCL Temporal Port — desktop app (Phase 1: real window, Neo-verified)

The local, installable counterpart to [CTCL](https://commoninstant.org) (Common
Temporal Coordinate Layer). Per the
[Temporal Port App whitepaper](../CTCL/docs/CTCL_Temporal_Port_App_通用時間端口技術白皮書_v0.1.md),
this is a **separate product on a separate stack** from the CTCL Worker — Rust core,
desktop-first, not a rewrite of the hosted API.

**License:** [Apache-2.0](LICENSE), same as [CTCL](https://github.com/kakon77777-commits/ctcl).

## Why Rust, why desktop-first

- The App's core job — a local API / background node other apps and agents call
  into — needs a persistent process and a loopback HTTP server. Mobile OSes
  aggressively suspend and kill background processes; that's a platform
  constraint, not a preference, so mobile is a later "companion," never first.
- Compiled Rust is also a genuine deterrent to casual reverse-engineering (no
  decompiler gets back anything close to readable source) — which turned out to
  matter more here than trying to build a custom-language moat for this
  particular product (checked EML's and PHOSPHOR's actual capabilities first:
  EML transpiles to plain unobfuscated Python and ships its own open-source
  reverse-transpiler; PHOSPHOR's custom VM has no I/O and no EML compilation
  path yet — neither is ready or even the right tool for this specific goal).
  See the whitepaper's own §17 "Desktop First" / §24 Phase 0-6 roadmap.

## Structure

- `ctcl-core/` — the reference-instant + heterogeneous-time-transformation logic:
  encodings (unix_s/ms/us/ns, rfc3339), timescales (utc/posix/tai/gps approx),
  and temporal systems (constant/piecewise/paused/table rate). Ported from the
  CTCL Worker's `src/worker.js` for **behavioral parity, verified**: the CLI's
  `convert` output is byte-identical to `commoninstant.org`'s `/v1/convert` for
  the same input.
- `ctcl-store/` — SQLite-backed persistence for instants, custom systems, and
  Temporal Groups. The local, offline equivalent of the Worker's `CTCL_KV`
  registry — same record shapes, same semantics (re-creating a group bumps its
  version; expanding a group across members isolates per-member errors so one
  bad system id doesn't fail the whole request), different storage engine.
- `ctcl-cli/` — the CLI: `now`, `convert`, `serve` (a local no-terminal web
  preview), and `instant`/`system`/`group` subcommands over `ctcl-store`.
- `ctcl-desktop/` — the **real Phase 1 desktop shell** (Tauri 2). Same
  `ctcl-core`/`ctcl-store` as the CLI, called through Tauri's IPC instead of
  HTTP. A genuine double-click-able window, not a browser preview.

## Develop

```bash
cargo build
cargo test              # 23 tests across ctcl-core + ctcl-store
cargo run --bin ctcl -- now
cargo run --bin ctcl -- convert --value 1783420000.5 --from unix_s --to rfc3339 --tz Asia/Taipei
cargo run --bin ctcl -- serve                       # opens a browser, no terminal needed after this
cargo run --bin ctcl -- instant register --label "handoff"
cargo run --bin ctcl -- system create --id user:game_world --epoch 1700000000 --rate 20
cargo run --bin ctcl -- group create --id group:demo --members "utc,tai,tz:Asia/Taipei,user:game_world"
cargo run --bin ctcl -- group expand group:demo
```

Or just double-click `Open-CTCL-Preview.bat` — no terminal needed at all.

For the real desktop app:

```bash
cargo tauri dev --manifest-path ctcl-desktop/Cargo.toml
```

## Status

**Phase 0 (Shared Core) complete**: core time math, SQLite persistence
(instants/systems/groups), the full CLI surface, and a local web preview are
all done and tested (23 unit tests + full manual CLI smoke test of every
subcommand, including real cross-process persistence and every error path).

**Phase 1 (Desktop MVP) started and Neo-verified 2026-07-12**: a real Tauri
window (title "CTCL Temporal Port") reusing the Phase 0 preview's UI, wired to
`ctcl-core` through Tauri commands instead of HTTP. This is the one thing that
genuinely needed a human's eyes rather than automated testing — Neo confirmed
the live clock and the convert flow (Taipei → Tokyo, Taipei → UTC) both
produce correct results in the actual running window. Still to come: the
Dashboard's Systems/Groups sections (backend commands already wired -
`list_systems`/`list_groups`/`expand_group` - just no UI yet), the local API +
capability-scoped permission model (Phase 2), device clock observation
(Phase 3).

This is intentionally **not** trying to replicate CTCL Web's whole surface at
once — it starts from the same core math and grows outward, same as the Worker
did.
