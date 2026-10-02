# `require_auth()` Call Site Audit

Closes the "Authorization hardening" item in `docs/pre-deploy-security-checklist.md`
(#45, tracked here as #263).  Updated in issue #411 to implement `require_auth_for_args`
on the solver-facing entrypoints.  Every `require_auth()` / `require_auth_for_args()`
call site in `intent_settlement/src/lib.rs` was reviewed for whether scoped auth
meaningfully reduces delegated-execution risk — i.e. the risk that a relayer or
invoker contract passing a solver's auth entry could redirect the signature to
unintended arguments.

## Upgraded to `require_auth_for_args` (issue #411)

| Function | Scoped args | Rationale |
|---|---|---|
| `submit_intent` | `(user, dst_token, min_dst_amount)` | Prevents a composable invoker from redirecting the user's signed submission to a different destination token or minimum output. |
| `accept_intent` | `(intent_id,)` | Prevents a delegating invoker from having a solver accept a different intent than the one the solver actually signed for. Scoping to `intent_id` is the minimal sufficient scope since the bond token is always the default for this entrypoint. |
| `accept_intent_with_bond` | `(intent_id, bond_token)` | Same as `accept_intent` plus the specific bond token, preventing redirection to a different intent or a different bond denomination. |
| `fill_intent` | `(solver, intent_id, fill_amount)` | Highest-value call site — the auth gates an outgoing token transfer. Prevents a delegating invoker from filling a different intent, or a different amount, than the solver signed for. The solver address is included so the tuple is globally unique (not just per-contract). |
| `begin_fill` | `(solver, intent_id, fill_amount)` | Same rationale as `fill_intent` — tokens move into escrow. Scoping prevents replay across intents or amounts. |
| `batch_accept_intent` | `(intent_ids,)` — the full `Vec<BytesN<32>>` | The solver's sig covers exactly this ordered set; replaying it for a different list of intents is rejected. |
| `batch_fill_intent` | `(fills,)` — the full `Vec<(BytesN<32>, i128)>` | Covers both the intent IDs and fill amounts; a signature for one fill list cannot be replayed for a different list. |

## Kept as `require_auth()`

| Function | Signer | Rationale |
|---|---|---|
| `initialize` | `admin` | One-time setup; the signer *is* the value being recorded as admin — no sub-scope to narrow. |
| `propose_fee_recipient` | stored `admin` | Single global admin capability; no meaningful sub-scope within "being admin". |
| `accept_fee_recipient` | `new_fee_recipient` | Recipient proves ownership of their own address; the timelock and pending-proposal match already constrain which proposal can be accepted. |
| `propose_admin_transfer` | stored `admin` | Same as `propose_fee_recipient`. |
| `accept_admin_transfer` | `new_admin` | Same as `accept_fee_recipient`. |
| `register_solver` | `solver` | Solver consents to locking their own bond funds; simple self-action with no delegated-execution surface. |
| `deregister_solver` | `solver` | Solver-only self-action. |
| `withdraw_bond` | `solver` | Solver-only self-action on their own bond. |
| `cancel_intent` | `user` | Simple "cancel my own intent" self-action; an explicit `intent.user != user` ownership check runs immediately after. |
| `request_extension` | `solver` | At most one extension per intent; no funds move and no cross-intent redirection is possible. |
| `require_admin` (helper) | `admin` | Uniform admin authority — no per-argument capability to scope. |
| `require_admin_or_pauser` (helper) | `admin` or `pauser` | Same as `require_admin`. |

## Integration impact

`require_auth_for_args` changes the signed-payload shape clients must build.

- **`submit_intent`:** user wallets must sign over `(user, dst_token, min_dst_amount)`.
- **`accept_intent`:** solver bots must sign over `(intent_id,)`.
- **`accept_intent_with_bond`:** solver bots must sign over `(intent_id, bond_token)`.
- **`fill_intent`:** solver bots must sign over `(solver, intent_id, fill_amount)`.
- **`begin_fill`:** solver bots must sign over `(solver, intent_id, fill_amount)`.
- **`batch_accept_intent`:** solver bots must sign over the full `Vec<BytesN<32>>` of intent IDs.
- **`batch_fill_intent`:** solver bots must sign over the full `Vec<(BytesN<32>, i128)>` of (intent_id, fill_amount) pairs.

See `docs/solver-integration-guide.md` for the updated payload shapes solver bot
authors must sign.  The Soroban SDK's `IntoVal` implementation serializes these
tuples in canonical XDR order, which is what on-chain auth verification expects.

## Negative-test coverage

Tests must verify that a `MockAuth` entry built for intent A is rejected when
submitted for intent B.  See `intent_settlement/src/test.rs` for the
`test_accept_intent_auth_scoping` and `test_fill_intent_auth_scoping` test cases
that build auth entries manually and confirm cross-intent replay fails with
`Unauthorized`.
