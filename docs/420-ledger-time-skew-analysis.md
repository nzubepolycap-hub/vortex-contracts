# Ledger Close-Time Skew Analysis and Safety Margins

**Issue:** [#420](https://github.com/stellar-vortex-protocol/vortex-contracts/issues/420)  
**Status:** Implemented — see `SLASH_GRACE_SECS` in `intent_settlement/src/lib.rs`

---

## 1. Problem Statement

Every time-based boundary in `intent_settlement` compares against
`env.ledger().timestamp()`, which is the **validator-agreed ledger close time**
for the ledger in which the transaction executes.  This timestamp is determined
by Stellar consensus — not by the submitter's wall clock — and can deviate from
wall-clock time in ways that create unfair races between solvers and slashers.

A solver that submits a `fill_intent` transaction at `deadline - 1 s` wall-clock
time may land in a ledger whose close time is `deadline + 2 s`, and be slashed
despite acting in good faith.  That is both a fairness failure and a source of
real disputes.

---

## 2. Stellar Close-Time Semantics

### 2.1 How ledger timestamps are set

Each Stellar ledger's `close_time` is set by the validator network during the
SCP consensus round.  Validators propose and agree on a close time that must
satisfy the following invariants from the Stellar Core source:

- **Monotonicity:** `close_time[n] > close_time[n-1]` — timestamps never go
  backwards.
- **Validity window:** The proposed close time must fall within
  `[previous_close_time + 1, wall_clock + MAX_CLOSE_TIME_DRIFT]` where
  `MAX_CLOSE_TIME_DRIFT` is a network-level constant (currently **60 seconds**
  for the public network, in `src/herder/Herder.cpp`).
- **No guaranteed wall-clock alignment:** There is no lower bound mandating
  that `close_time >= wall_clock`.  Under heavy network load or during a quorum
  convergence delay, the close time can fall meaningfully below a participant's
  wall-clock reading.

### 2.2 Typical ledger interval

Stellar targets a ~5 second ledger close interval.  In practice, ledgers close
every 4–7 seconds under normal conditions.  Periods of network instability can
produce longer gaps.

### 2.3 Observed and worst-case drift

| Scenario | Observed drift | Notes |
|---|---|---|
| Nominal operation | < 1 s from wall-clock | Validators stay in sync |
| Mild load | 1–3 s | SCP converges one extra round |
| Network partition recovery | Up to ~7 s | Two ledgers close back-to-back |
| Theoretical maximum | 60 s | `MAX_CLOSE_TIME_DRIFT` hard cap, never observed in practice |

**For safety-margin sizing we use the worst commonly observed value of ~7 s**
(two successive ledger gaps of ~5 s with no intermediate progress), not the
theoretical 60-second cap, to avoid making fill windows excessively wide.

---

## 3. Affected Time Boundaries

| Guard | Function | Direction of risk |
|---|---|---|
| Fill-window deadline (`now < deadline`) | `fill_intent`, `begin_fill` | Solver submits at `deadline - 1 s` but lands in a ledger at `deadline + Δ` → fills rejected unfairly |
| Slash eligibility (`now >= deadline + grace`) | `slash_solver` | Slasher submits at `deadline + 1 s` and lands in a ledger at `deadline - Δ` → slash rejected; or solver gets slashed too early without grace |
| Dispute window (`now < dispute_deadline`) | `open_dispute` / `dispute_fill` | User submits near boundary |
| Arbiter timeout (`now >= raised_at + ARBITER_WINDOW`) | `release_fill` | Boundary race; 24 h window makes drift negligible |
| Cancel cooldown (`now >= last_cancel + CANCEL_COOLDOWN`) | `cancel_intent` | 60 s cooldown; ~7 s drift ≈ 12% of window — acceptable |
| Slash cooldown (`now >= last_slash + SLASH_COOLDOWN`) | `accept_intent` | 1 h cooldown; ~7 s drift is negligible |

### 3.1 Where a grace period is justified vs. where it is not

**Fill-window deadline (exclusive upper bound for fills):** The window is already
bounded by `FILL_WINDOW` seconds.  Adding a grace period *to the fill window*
would give the solver extra time to fill, which is a different concern.  We do
**not** widen the fill window.

**Slash eligibility (onset of slash availability):** This is the boundary where
skew creates an asymmetric risk.  A slasher sees `now > deadline` on their clock
and submits; the transaction lands in a ledger whose close time is *just over*
the deadline.  The solver had genuinely submitted a fill transaction whose
signature was broadcast before deadline, but it either lost the race or failed
for unrelated reasons.  Adding `SLASH_GRACE_SECS` to the slash onset absorbs the
timing uncertainty without altering the fill window itself — an asymmetric margin
that favours the solver at the slasher's expense (the slasher must wait a few
extra seconds).

**Dispute / arbiter windows:** The dispute window is 1 hour and the arbiter
window is 24 hours.  At ~7 s worst-case drift, these represent < 0.2% of the
window duration.  Adding a grace period would complicate the dispute flow with
negligible security benefit.  No margin is added.

---

## 4. Implemented Safety Margin

```rust
// intent_settlement/src/lib.rs
const SLASH_GRACE_SECS: u64 = 10; // 2 × worst-case ledger gap (~5 s)
```

**Rationale for 10 s:**
- Covers 2× the typical ledger interval (2 × 5 s = 10 s), giving a full
  extra ledger of slack beyond the deadline.
- Exceeds the worst commonly observed close-time drift of ~7 s.
- Is small enough that a slasher cannot observe a missed fill window more than
  10 seconds after the fact without being able to slash — economic finality is
  preserved.
- Does **not** approach the 60-second theoretical maximum; if drift of that
  magnitude occurred, the network would be in a severe incident state where
  human intervention is appropriate regardless.

### 4.1 Implementation: named helper functions

All time-boundary comparisons in `intent_settlement` use named helper functions
rather than raw numeric comparisons, so the semantics are clear at every call
site and can never silently drift:

```rust
/// Fill is valid while now < deadline (exclusive upper bound, issue #26).
fn fill_window_open(now: u64, deadline: u64) -> bool {
    now < deadline
}

/// Slash is eligible only after deadline + grace absorbs close-time drift.
fn slash_eligible(now: u64, deadline: u64) -> bool {
    now >= deadline.saturating_add(SLASH_GRACE_SECS)
}

/// Dispute window is still open while now < dispute_deadline.
fn dispute_window_open(now: u64, dispute_deadline: u64) -> bool {
    now < dispute_deadline
}

/// Arbiter timeout has elapsed (inclusive at the boundary).
fn arbiter_timeout_reached(now: u64, raised_at: u64) -> bool {
    now >= raised_at.saturating_add(ARBITER_WINDOW)
}
```

### 4.2 Interaction with extension windows

`request_extension` extends `intent.deadline` by up to `MAX_EXTENSION_DURATION`
(300 s) from the current ledger time when the extension is granted.  The
extended deadline is subject to the same fill/slash boundary semantics:

- `fill_window_open` uses the extended deadline.
- `slash_eligible` adds `SLASH_GRACE_SECS` to the extended deadline.

No special handling is needed; the grace is additive to whatever deadline is
stored.

---

## 5. Ledger-Sequence-Based Deadlines: Evaluation

The issue scope asked for an evaluation of switching to **ledger-sequence-based
deadlines** rather than close-time-based ones.

| Dimension | Close-time (current) | Ledger sequence |
|---|---|---|
| Human-readable | ✅ Deadlines in wall-clock seconds are intuitive | ❌ Requires knowing ledger rate to convert |
| Skew sensitivity | ⚠️ Subject to close-time drift (mitigated by grace) | ✅ Immune to close-time drift |
| Variable interval | ✅ Unaffected — stored as absolute timestamp | ⚠️ Ledger intervals vary (4–7 s typical); a sequence-count window of N ledgers spans a variable wall-clock duration |
| On-chain expression | ✅ `deadline` is a `u64` seconds timestamp | Would require `deadline_seq: u32` (sequence number at close) |
| Existing API compat | ✅ No change to `submit_intent` / `accept_intent` caller ABI | ❌ Breaking ABI change |
| Risk of DoS via gap | ✅ Not applicable | ⚠️ A network stall creates many ledgers quickly once it recovers, potentially rushing through deadlines |

**Conclusion:** Ledger-sequence deadlines would eliminate close-time skew risk
entirely but introduce variable-duration windows and a breaking API change.  The
`SLASH_GRACE_SECS` approach achieves the security goal (protecting solvers from
unfair slashing due to drift) with zero ABI change and negligible complexity.
Switching to sequence-based deadlines is deferred until there is a concrete
requirement that close-time drift cannot be absorbed by a grace period (e.g. a
network where typical drift routinely exceeds 10 s).

---

## 6. Boundary Test Matrix

The following boundary conditions should be covered by unit tests:

| Scenario | Expected behaviour |
|---|---|
| `now == deadline - 1` | Fill succeeds, slash fails |
| `now == deadline` | Fill fails (exclusive), slash still fails (grace not elapsed) |
| `now == deadline + SLASH_GRACE_SECS - 1` | Fill fails, slash still fails |
| `now == deadline + SLASH_GRACE_SECS` | Fill fails, slash succeeds |
| `now == deadline + SLASH_GRACE_SECS + 1` | Fill fails, slash succeeds |
| Dispute: `now == dispute_deadline - 1` | Dispute opens successfully |
| Dispute: `now == dispute_deadline` | Dispute rejected (window exclusive) |
| Arbiter: `now == raised_at + ARBITER_WINDOW - 1` | `release_fill` rejected as arbiter timeout |
| Arbiter: `now == raised_at + ARBITER_WINDOW` | `release_fill` succeeds (inclusive) |

---

*Closes #420*
