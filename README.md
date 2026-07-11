# CTCL Temporal Port — desktop app (Phase 0)

The local, installable counterpart to [CTCL](https://commoninstant.org) (Common
Temporal Coordinate Layer). Per the
[Temporal Port App whitepaper](../CTCL/docs/CTCL_Temporal_Port_App_通用時間端口技術白皮書_v0.1.md),
this is a **separate product on a separate stack** from the CTCL Worker — Rust core,
desktop-first (Tauri, not yet added), not a rewrite of the hosted API.

**License:** [Apache-2.0](LICENSE), same as [CTCL](https://github.com/kakon77777-commits/ctcl).

## Why Rust, why desktop-first

- The App's core job — a local API / background node other apps and agents call
  into — needs a persistent process and a loopback HTTP server. Mobile OSes
  aggressively suspend and kill background processes; that's a platform
  constraint, not a preference, so mobile is a later "companion," never first.
- Compiled Rust is also a genuine deterrent to casual reverse-engineering (no
  decompiler gets back anything close to readable source) — which turned out to
  matter more here than trying to build a custom-language moat for this
  particular product. See the whitepaper's own §17 "Desktop First" / §24 Phase 0-6
  roadmap; this repo starts at Phase 0.

## Structure

- `ctcl-core/` — the reference-instant + heterogeneous-time-transformation logic:
  encodings (unix_s/ms/us/ns, rfc3339), timescales (utc/posix/tai/gps approx),
  and temporal systems (constant/piecewise/paused/table rate). Ported from the
  CTCL Worker's `src/worker.js` for **behavioral parity, verified**: the CLI's
  `convert` output is byte-identical to `commoninstant.org`'s `/v1/convert` for
  the same input.
- `ctcl-cli/` — the Phase 0 CLI (`ctcl now`, `ctcl convert`) — a thin surface over
  `ctcl-core`, mirroring `GET /v1/now` and `POST /v1/convert`.

## Develop

```bash
cargo build
cargo test
cargo run --bin ctcl -- now
cargo run --bin ctcl -- convert --value 1783420000.5 --from unix_s --to rfc3339 --tz Asia/Taipei
```

## Status

Phase 0 (Shared Core) in progress: core time math + CLI done and tested (13
tests). Still to come, per the whitepaper's own roadmap: SQLite-backed
persistent Temporal Systems/Groups (today's core is pure functions, no storage
yet), a Tauri desktop shell (Phase 1), the local API + capability-scoped
permission model (Phase 2), device clock observation (Phase 3).

This is intentionally **not** trying to replicate CTCL Web's whole surface at
once — it starts from the same core math and grows outward, same as the Worker
did.
