# CTCL Temporal Port — desktop app (Phase 1 Dashboard complete: Systems & Groups UI)

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
- `ctcl-desktop/` — the **real desktop shell** (Tauri 2). Same
  `ctcl-core`/`ctcl-store` as the CLI, called through Tauri's IPC instead of
  HTTP. A genuine double-click-able window, not a browser preview. Also owns
  three background threads that only run when explicitly enabled (all off by
  default, whitepaper §7.2 "default off"): `local_api.rs` (Phase 2 — a
  loopback-only HTTP gateway, bearer-token auth, capability-scope enforced,
  every call audit-logged), `device_observer.rs` (Phase 3 — periodically
  samples the wall clock against a monotonic anchor and classifies drift /
  sleep-wake / manual clock rollback; only anomalies are persisted), and
  `trigger_engine.rs` (Phase 4 — polls due triggers and dispatches their
  action through a pluggable `ActionDispatcher`).

## Develop

```bash
cargo build
cargo test               # 64 tests across ctcl-core + ctcl-store + ctcl-desktop
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

**Phase 1 (Desktop MVP) Neo-verified 2026-07-12**: a real Tauri window (title
"CTCL Temporal Port") reusing the Phase 0 preview's UI, wired to `ctcl-core`
through Tauri commands instead of HTTP. This is the one thing that genuinely
needed a human's eyes rather than automated testing — Neo confirmed the live
clock and the convert flow (Taipei → Tokyo, Taipei → UTC) both produce correct
results in the actual running window.

**Phase 2 (Local Gateway) Neo-verified 2026-07-12**: the loopback HTTP API
(`local_api.rs`) plus a Settings tab generated dynamically from the backend's
scope list and feature-status data, so the UI can't silently drift from what's
real. 6 real socket-level integration tests (raw `TcpStream`, not mocks).

**Phase 3 (Device Clock Observer) shipped 2026-07-12**: a background sampling
thread (`device_observer.rs`) compares the system wall clock against a
monotonic anchor on a configurable interval (default 20s) and classifies the
gap as `normal` / `drift` / `sleep_wake` / `rollback`
(`ctcl_store::device_observer::classify_gap`, a pure function with its own
unit tests — no OS-specific sleep/wake hooks needed, since a wall-clock gap
that vastly exceeds the requested interval is itself a platform-independent
signal that the process wasn't continuously running). Only anomalies are
persisted; the *current* status (including "everything is fine") is a
separate, in-memory concern read live by the Settings UI. Exposed three ways,
consistently: a Tauri command (`device_observer_status` /
`list_device_events`), a Local API route (`GET /v1/device-events`, gated by
the `device_clock.read` scope that Phase 2 had already reserved), and a new
Settings card. Verified with real background-thread tests across real
wall-clock time (not mocked) plus classify_gap unit tests covering every
branch, including the cross-platform ambiguity in whether a monotonic clock's
elapsed reading includes suspended time. **The Settings UI card itself is
implemented but not yet eyeballed in a real window** — same discipline as
every prior UI change in this project: backend fully tested automatically,
visual confirmation of an actual native Tauri window is left for Neo.

**Phase 4 (Trigger Engine) shipped 2026-07-14**, whitepaper §4.3/§9.4:
$I^*=I_{\text{target}} \Rightarrow \text{action}$ (kind `common_instant`, an
absolute deadline) or $\tau_{\text{custom}}=\tau_{\text{target}} \Rightarrow
\text{action}$ (kind `custom_time`, relative to a stored temporal system's
local time, e.g. an agent's active-time clock). Both reduce to one comparison
in `ctcl_store::trigger::Store::due_triggers`: is a live numeric value
(wall-clock unix seconds, or the named system's current local seconds via the
already-existing `system_now`) `>=` or `<=` a target. `==` is deliberately not
offered — under periodic polling it would almost always step over an exact
instant and silently never fire, a footgun rather than a feature. A trigger
fires exactly once (`active` → `fired`), and only after its action actually
succeeds — a failed dispatch leaves it `active` for retry next tick rather
than being marked fired without the action happening.

Two action kinds: `notification` (logged; no OS toast/system-tray integration
yet, honestly not claimed) and `callback` (hands a URI to the OS's own default
handler — `start`/`open`/`xdg-open` — so whatever app owns that scheme decides
what happens next; CTCL does not register or resolve schemes itself, matching
§7.1's "private scheme only" scope for this phase). Dispatch is behind an
`ActionDispatcher` trait so automated tests never actually open a URI or spawn
a process — a `FakeDispatcher` records calls instead, while the real desktop
app uses `RealDispatcher`. Exposed the same three consistent ways as Phase 3:
Tauri commands (`create_trigger`/`list_triggers`/`cancel_trigger`), a Local
API route (`GET /v1/triggers`, gated by the already-reserved `triggers.read`
scope), and a Settings card (create form + live list + cancel buttons).
14 new tests (9 condition/persistence tests in `ctcl-store`, including a
countdown-style `<=` case; 4 dispatch tests in `ctcl-desktop` including one
real background-thread firing over real wall-clock time; 1 new scope-gated
Local API route test). Same discipline as every prior UI change: backend
fully tested automatically, the Settings card's actual rendering in a native
window is left for Neo.

**Dashboard: Systems & Groups UI shipped 2026-07-14** — this closes the last
piece of Phase 1's own roadmap item list (`Dashboard; Convert; Systems;
Groups; Local API; URI Scheme` — Systems/Groups had been backend-only debt
carried since Phase 1). A new third tab ("時鐘與群組") alongside Home and
Settings: a **Custom Systems** card (create a constant-rate system — id,
epoch, rate, offset, matching `ctcl-cli`'s own `system create` scope, which is
also constant-rate-only; piecewise/paused/table systems remain CLI/API-only)
with a live list showing each system's current local seconds, and a
**Temporal Groups** card (create — id, comma-separated members — with a live
list and a per-group "展開/expand" button that projects the current instant
across every member inline). New Tauri commands: `create_system`/`get_system`
(bundles the stored record with a live `system_now` evaluation),
`create_group`/`get_group` (both thin wrappers over already-tested
`ctcl-store` methods — no new store logic, so the existing 64 tests cover the
underlying behavior unchanged; this increment is UI wiring, not new backend
capability). One real naming gotcha caught before it could become a silent
runtime bug: Tauri's default JS-camelCase → Rust-snake_case argument
conversion is easy to get wrong for a multi-word parameter with no way to
verify it interactively (no native-window test tool) — `create_system`
explicitly declares `#[tauri::command(rename_all = "snake_case")]` and the JS
side passes `epoch_parent_sec` verbatim, so there's no implicit-conversion
assumption to get wrong, matching the explicit-match discipline
`create_trigger`'s struct argument already required.

Still to come: Phase 5 (team sync — note this needs an actual sync-backend
*product* decision, e.g. self-hosted vs. hub on commoninstant.org vs. a new
paid tier, not just more local Rust code, so it's being left for Neo to weigh
in on rather than architected unilaterally) and Phase 6 (mobile companion,
explicitly last per the whitepaper's own ordering).

This is intentionally **not** trying to replicate CTCL Web's whole surface at
once — it starts from the same core math and grows outward, same as the Worker
did.
