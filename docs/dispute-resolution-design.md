# Dispute-Resolution Flow Design

Tracking issue: [#48](https://github.com/stellar-vortex-protocol/vortex-contracts/issues/48)

**Arbiter Selection Process:** See [`docs/arbiter-election-process.md`](./arbiter-election-process.md) (issue #309) for how the community nominates and endorses arbiter candidates. This document describes dispute resolution mechanics; arbiter-election describes who serves on the committee.

**Arbiter Panel (issue #407):** The single-arbiter role described below is
replaced by an m-of-n bonded arbiter panel. See
[Arbiter panel](#arbiter-panel-m-of-n-bonded-arbiters) for the panel contract,
selection, voting, and tally rules.

---

## Problem statement

Once `fill_intent` succeeds the intent moves to state `Filled` — permanently.
There is currently no path for a user to contest a fill that technically met
`min_dst_amount` but that the user believes was manipulated, misdirected, or
otherwise incorrect.

The contract cannot independently verify off-chain or cross-chain facts (the
source-chain transaction is not proven on-chain; Vortex's security model
currently trusts solver bonds rather than cryptographic proofs). Any dispute
mechanism must therefore acknowledge that:

1. The contract **can** verify: token balances received, fill amounts, intent
   state, timestamps, and which solver signed the fill.
2. The contract **cannot** verify: whether the source-chain tx actually
   occurred, whether the quoted rate was fair, or whether the off-chain solver
   behaviour was honest.

---

## Scope: what counts as a valid on-chain dispute?

Given the above constraints, a dispute is valid when **at least one** of the
following is true:

| Category | Example | On-chain verifiable? |
|---|---|---|
| **Underfill** | `fill_amount < min_dst_amount` stored in the intent | ✅ already blocked by `fill_intent` guard — not disputable post-fill |
| **Wrong recipient** | Output tokens sent to an address other than `intent.user` | ✅ verifiable via token events / balance diff at fill time |
| **Duplicate fill** | A second fill after `Filled` state is set | ✅ already blocked — not disputable |
| **Stale-rate claim** | User claims the rate was manipulated off-chain | ❌ not verifiable on-chain — out of scope for v1 |
| **Source-chain non-delivery** | User claims source-chain tx never happened | ❌ not verifiable without a cross-chain oracle — deferred to oracle integration milestone |

**v1 dispute scope:** A dispute window is provided for a user to flag potential
wrong-recipient or off-chain-integrity concerns. Resolution is performed by a
bonded m-of-n arbiter panel (see below). The on-chain mechanism escrows the
fill amount during the dispute window rather than immediately releasing it,
making the panel's decision enforceable.

---

## Proposed state machine addition

```
Open → Accepted → Filling (new) → Filled
                              ↘→ Disputed → Resolved (Upheld | Dismissed)
```

### New states

| State | Description |
|---|---|
| `Filling` | Solver has called `begin_fill`; output tokens are held in escrow by the contract. The user has a dispute window to contest. |
| `Disputed` | User raised a dispute during the window; fill is on hold. |
| `Resolved` | Arbiter panel closed the dispute. Sub-outcome stored separately. |

### New fields on `IntentRecord`

```rust
pub dispute_deadline: Option<u64>,  // escrow window end; set by begin_fill
pub dispute_raised_at: Option<u64>, // timestamp of open_dispute call
pub resolution: Option<DisputeResolution>, // Upheld | Dismissed
```

```rust
pub enum DisputeResolution {
    Upheld,    // panel sided with user; tokens returned to user, solver slashed
    Dismissed, // panel sided with solver; tokens released from escrow to user normally
}
```

---

## Fund flow

### Without a dispute (happy path)

```
solver --[fill_amount]--> contract escrow (begin_fill)
  [dispute_window elapses with no dispute]
anyone calls release_fill(intent_id)
contract escrow --[fill_amount]--> user
contract --[fee]--> fee_recipient  (deducted from fill, same as current)
```

### With a dispute: panel upholds the user

```
solver --[fill_amount]--> contract escrow (begin_fill)
user calls open_dispute(intent_id)
panel calls resolve_dispute(intent_id, Upheld)
  contract escrow --[fill_amount]--> user   (user made whole)
  solver bond slashed 10 % (same as slash_solver)
  intent re-opened for a new solver  OR  intent set to Resolved/Expired
```

### With a dispute: panel dismisses (solver wins)

```
solver --[fill_amount]--> contract escrow (begin_fill)
user calls open_dispute(intent_id)
panel calls resolve_dispute(intent_id, Dismissed)
  contract escrow --[fill_amount]--> user   (user still receives tokens)
  no slash; intent state = Resolved(Dismissed)
  solver bond unlocked
```

> **Note:** In both outcomes the user receives the tokens. The dispute only
> determines whether the solver is slashed for alleged misconduct.

---

## New entry-points (sketch)

```rust
/// Solver transfers fill_amount into contract escrow; starts dispute window.
/// Replaces the direct transfer in fill_intent once this design is implemented.
pub fn begin_fill(env: Env, solver: Address, intent_id: BytesN<32>, fill_amount: i128);

/// User opens a dispute within the dispute window.
/// Only callable while state == Filling and now < dispute_deadline.
pub fn open_dispute(env: Env, user: Address, intent_id: BytesN<32>);

/// Arbiter panel resolves a dispute. Triggers fund release and optional slash.
/// Only callable by the arbiter_panel contract while state == Disputed.
pub fn resolve_dispute(env: Env, arbiter: Address, intent_id: BytesN<32>, resolution: DisputeResolution);

/// Permissionless: release escrow to user after dispute window closes without a dispute.
pub fn release_fill(env: Env, intent_id: BytesN<32>);
```

---

## Arbiter panel (m-of-n bonded arbiters)

Issue #407 replaces the single arbiter with an `arbiter_panel` contract. The
settlement contract no longer trusts one address: `resolve_dispute` accepts a
resolution only from the registered panel contract.

### Registration and bond

- `register_arbiter(arbiter, bond)` — an arbiter stakes a bond (in the
  settlement token) to join the panel. The bond is held by the panel contract.
- `deregister_arbiter(arbiter)` — allowed only when the arbiter has no open
  votes; returns the remaining bond.
- Only bonded arbiters are eligible for selection.

### Panel selection

- Per dispute, the panel is drawn from the bonded set using `env.prng()` seeded
  with `intent_id`.
- **Documented limitation:** the ledger PRNG is manipulable by the ledger
  closer, so panel selection is *random-ish*, not cryptographically fair. It is
  therefore unsuitable as a sole defence; it is combined with bonds, public
  votes, and the timeout fallback below.
- **Exclusions:** an arbiter who is also the solver or the user for that intent
  is excluded from the panel.
- **Fewer than n available:** if fewer than `n` eligible arbiters exist, the
  panel is filled with all eligible arbiters and the threshold `m` is scaled
  down proportionally (never below 1). If no eligible arbiter exists, the
  dispute falls through to the `ARBITER_WINDOW` timeout rule.

### Voting window and public votes

- `open_vote(intent_id)` selects the panel and opens a voting window
  (`VOTE_WINDOW`, proposed 86400 s / 24 h).
- `vote(arbiter, intent_id, resolution)` records a public on-chain vote. Each
  selected arbiter may vote once; votes are emitted as events for full
  auditability.

### Tally and settlement

- `tally(intent_id)` counts votes. When `m`-of-`n` votes agree, the panel calls
  `resolve_dispute` on the settlement contract with the majority outcome.
- **Bond slashing:** arbiters who voted against the final outcome, or who did
  not vote at all, lose part of their bond. The slashed portion is split into
  the dispute-bond split (out of scope: arbiter rewards beyond this split).
- **Timeout fallback:** if the voting window elapses without reaching `m`
  votes, the existing `ARBITER_WINDOW` rule applies — a permissionless timeout
  releases escrow to the user (conservative default).

---

## Arbiter role

**v1 (superseded by #407):** The `admin` address acted as arbiter. This was the
simplest safe option for testnet — it required no new storage key or governance
mechanism.

**v2 (current):** The `arbiter_panel` contract holds the arbiter role. The
settlement contract stores the panel contract address (settable by admin via
`set_arbiter_panel(env, panel: Address)`) and accepts `resolve_dispute` only
from it. The panel is an m-of-n bonded committee; see
[Arbiter panel](#arbiter-panel-m-of-n-bonded-arbiters).

**Arbiter governance:** See [`docs/arbiter-code-of-conduct.md`](./arbiter-code-of-conduct.md)
(issue #300) for the complete governance policy, eligibility criteria, conflict-of-interest
disclosure requirements, recusal procedures, and decision-rationale standards that arbiters
must follow.

**Out of scope for this design:** fully trustless arbitration (requires a
cross-chain proof oracle).

---

## Dispute window

| Parameter | Proposed value | Rationale |
|---|---|---|
| `DISPUTE_WINDOW` | 3600 s (1 hour) | Long enough for the user to notice and act; short enough not to hold solver capital indefinitely. Adjustable by governance. |
| `VOTE_WINDOW` | 86400 s (24 hours) | Window for the selected panel to cast votes before the tally. |
| `ARBITER_WINDOW` | 86400 s (24 hours) | After a dispute is raised, the panel has 24 hours to resolve. If unresolved, a permissionless timeout releases escrow to the user (conservative default). |

---

## Security considerations

- **Griefing:** A user could open spurious disputes to delay solver capital
  release. Mitigation: require a small dispute bond from the user (e.g. 1 USDC),
  returned on Upheld, forfeited on Dismissed. Defer to follow-up issue.
- **Arbiter capture:** A single colluding arbiter can no longer decide alone;
  `m`-of-`n` votes are required and dissenting/non-voting arbiters lose bond.
  Residual risk: a majority of the panel colluding — mitigated by bonds, public
  votes, and the timeout fallback.
- **PRNG manipulation:** panel selection uses `env.prng()` seeded with
  `intent_id`; the ledger closer can influence it. Documented above as a known
  limitation, not a sole defence.
- **Escrow risk:** Tokens sit in the contract during the window. The contract
  must not be upgradeable without governance while escrowing user funds.
- **Re-entrancy:** `begin_fill` uses `transfer_from(solver → contract)`;
  `release_fill` and `resolve_dispute` use `transfer(contract → user)`. Ensure
  state is updated before token transfers (checks-effects-interactions).

---

## Implementation plan (follow-up issues)

1. Add `Filling` / `Disputed` / `Resolved` states and new `IntentRecord` fields.
2. Implement `begin_fill` + escrow logic (replaces direct transfer in `fill_intent`).
3. Implement `open_dispute` + `resolve_dispute` (panel-only caller).
4. Implement the `arbiter_panel` contract: register/bond, PRNG panel selection,
   voting window, m-of-n tally, bond slashing, and timeout fallback.
5. Tests: full dispute lifecycle with the panel, including a collusion scenario.
