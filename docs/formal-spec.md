# Vortex Protocol — Formal Specification

*Issue #414 — Quint specification of the intent state machine*

---

## Overview

`spec/vortex.qnt` is a formal model of the Vortex intent lifecycle written in
[Quint](https://github.com/informalsystems/quint), a lightweight specification
language built on top of TLA+. The model is checked for safety and liveness
using either the Quint **simulator** (`quint run`) or the
[Apalache](https://apalache-mc.org/) model checker (`quint verify`).

---

## Running the model checker

### Prerequisites

```bash
# Node ≥ 18 required
npm install -g @informalsystems/quint
# Optional: Apalache for full model-checking (bounded verification)
# See https://apalache-mc.org/docs/apalache/installation.html
```

### Simulate the named traces (fast, randomised)

```bash
cd spec
quint run --main=traces vortex.qnt
```

### Check safety invariants with Apalache (exhaustive, bounded)

```bash
# Check all safety invariants up to 15 steps
quint verify --main=vortex \
  --invariant=safety \
  --max-steps=15 \
  vortex.qnt
```

### Check liveness properties

```bash
quint verify --main=vortex \
  --temporal=live_intentTerminates \
  --temporal=live_solverCanAccept \
  --max-steps=30 \
  vortex.qnt
```

---

## State machine coverage

The spec covers every `IntentState` variant and transition shown in the
README lifecycle diagram:

| Transition | Action | Pre-condition |
|---|---|---|
| `[*] → Open` | `submitIntent` | deadline in the future |
| `Open → Accepted` | `acceptIntent` | solver eligible, deadline not reached |
| `Open → Cancelled` | `cancelIntent` | caller == intent.user |
| `Open → Expired` | `expireIntent` | `tick >= deadline` |
| `Accepted → Filled` | `fillIntent` | `fillAmt >= minDstAmount`, within fill window |
| `Accepted → PartiallyFilled` | `partialFill` | partial fill, within fill window |
| `Accepted → Open` | `slashSolver` | fill window expired |
| `Accepted → Filling` | `beginFill` | within fill window |
| `Filling → Filled` | `releaseFill` | dispute window elapsed |
| `Filling → Disputed` | `disputeFill` | within dispute window, caller == user |
| `Disputed → Resolved` | `resolveDispute` | arbiter call |
| `Disputed → Resolved` | `arbiterTimeout` | arbiter window expired |

`Bidding` is reserved in the spec but not produced by any current action (matching the
contract, where `submit_intent` always opens intents as `Open`).

---

## Safety invariants (plain English)

| ID | Name | Property |
|---|---|---|
| INV-1 | `inv_filledIsTerminal` | A `Filled` intent is terminal and can never re-enter an active state. |
| INV-2 | `inv_bondNonNegative` | A solver's bond can reach zero through slashing but can never become negative. |
| INV-3 | `inv_noStuckIntents` | Every `Accepted` intent has a fill deadline ≤ `MAX_TICK`; no intent is permanently stuck. |
| INV-4 | `inv_acceptedHasSolver` | Every intent in `Accepted` state has a solver assigned (solver ≥ 0). |
| INV-5 | `inv_filledAmountSufficient` | A `Filled` intent always has `fillAmount ≥ minDstAmount`. |
| INV-6 | `inv_treasuryMonotone` | The treasury balance only ever increases; slash and fee proceeds flow in, never out. |
| INV-7 | `inv_cancelOnlyOpen` | `Cancelled` is only reachable from `Open`; an `Accepted` intent cannot be cancelled directly. |
| INV-8 | `inv_resolvedHasOutcome` | A `Resolved` intent always carries a `DisputeResolution` outcome (`Upheld` or `Dismissed`). |

All eight invariants are checked by Apalache for the bounded model
(2 users, 2 solvers, 2 intents, ticks 0..30).

---

## Liveness properties (plain English)

| ID | Name | Property |
|---|---|---|
| LIVE-1 | `live_intentTerminates` | Every submitted intent *eventually* reaches a terminal state (Filled, Cancelled, Expired, Slashed, or Resolved) within the tick bound. |
| LIVE-2 | `live_solverCanAccept` | An `Open` intent whose deadline has not passed *always eventually* transitions out of `Open` (a registered solver can always claim it). |

---

## Bounding strategy

To avoid combinatorial state explosion while still covering all transitions:

- **2 users** (indexes 0–1) and **2 solvers** (indexes 0–1)
- **2 intents maximum** at any time (`MAX_INTENTS = 2`)
- **Amounts** bounded to 1..10 (small integers)
- **Time** modelled as discrete ticks 0..30 (`MAX_TICK = 30`)
- **Cross-chain proof semantics** abstracted as a non-deterministic boolean oracle
  (not modelled; the fill action simply accepts any `fillAmt ≥ minDstAmount`)

---

## Trace-conformance harness

`intent_settlement/tests/conformance.rs` contains six Rust integration tests
that replay the named traces from `spec/vortex.qnt` against the real contract:

| Rust test | Quint trace | Transition exercised |
|---|---|---|
| `conformance_happy_path` | `happyPath` | Open → Accepted → Filled |
| `conformance_cancel_path` | `cancelPath` | Open → Cancelled |
| `conformance_expire_path` | `expirePath` | Open → Expired |
| `conformance_slash_path` | `slashPath` | Accepted → Open (slash re-open) |
| `conformance_dispute_upheld` | `disputeUpheldPath` | Accepted → Filling → Disputed → Resolved (slash) |
| `conformance_dispute_dismissed` | `disputeDismissedPath` | Accepted → Filling → Disputed → Resolved (no slash) |

Run the harness:

```bash
cd intent_settlement
cargo test --test conformance
```

---

## Files

| File | Purpose |
|---|---|
| `spec/vortex.qnt` | Quint specification (state machine + invariants + traces) |
| `intent_settlement/tests/conformance.rs` | Rust trace-conformance harness |
| `docs/formal-spec.md` | This document |
