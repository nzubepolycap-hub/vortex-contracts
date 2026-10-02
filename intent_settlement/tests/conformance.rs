//! Conformance harness — Issue #414
//!
//! Replays the named traces from `spec/vortex.qnt` against the real
//! `IntentSettlement` contract running in the Soroban test environment.
//! Each test drives the contract through one path of the state-machine and
//! asserts the safety invariants documented in `docs/formal-spec.md`.
//!
//! ## Generating ITF trace files (optional)
//!
//! ```bash
//! npm install -g @informalsystems/quint
//! quint run --main=traces spec/vortex.qnt --out-itf spec/traces/happy_path.itf.json
//! # … repeat for cancel_path, expire_path, slash_path, dispute_upheld, dispute_dismissed
//! ```
//!
//! ## Running
//!
//! ```bash
//! cd intent_settlement && cargo test --test conformance --features testutils
//! ```

#![cfg(test)]

use vortex_intent_settlement::{
    DisputeResolution, IntentSettlement, IntentSettlementClient, IntentState,
};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, BytesN, Env, String,
};

// ── Constants (mirror compile-time defaults in lib.rs) ────────────────────────

/// Solver bond: 100 USDC at 7 Stellar decimals.
const BOND: i128 = 100 * 10_000_000;

/// Minimum acceptable destination amount: 3.5 USDC.
const MIN_DST: i128 = 35_000_000;

/// A valid fill amount that clears MIN_DST: 3.6 USDC.
const FILL: i128 = 36_000_000;

/// Protocol fee in bps (PROTOCOL_FEE_BPS = 5 in lib.rs).
const FEE_BPS: i128 = 5;

/// Ethereum src_chain identifier.
const SRC_CHAIN: &str = "ethereum";

/// A valid ERC-20 token address (format-validated on-chain for Ethereum).
const SRC_TOKEN: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

/// Arbitrary source amount (positive non-zero, within MAX_AMOUNT).
const SRC_AMT: i128 = 1_000_000_000i128;

// ── Fixture ───────────────────────────────────────────────────────────────────

struct Ctx {
    env:           Env,
    admin:         Address,
    fee_recipient: Address,
    user:          Address,
    solver:        Address,
    contract_id:   Address,
    bond_token:    Address,
    dst_token:     Address,
}

impl Ctx {
    fn client(&self) -> IntentSettlementClient<'_> {
        IntentSettlementClient::new(&self.env, &self.contract_id)
    }

    fn bond_admin(&self) -> token::StellarAssetClient<'_> {
        token::StellarAssetClient::new(&self.env, &self.bond_token)
    }

    fn dst_admin(&self) -> token::StellarAssetClient<'_> {
        token::StellarAssetClient::new(&self.env, &self.dst_token)
    }

    fn pass_time(&self, secs: u64) {
        self.env.ledger().with_mut(|li| li.timestamp += secs);
    }

    fn register_solver(&self) {
        self.bond_admin().mint(&self.solver, &BOND);
        self.client().register_solver(&self.solver, &BOND);
    }

    fn submit(&self) -> BytesN<32> {
        self.client().submit_intent(
            &self.user,
            &String::from_str(&self.env, SRC_CHAIN),
            &String::from_str(&self.env, SRC_TOKEN),
            &SRC_AMT,
            &self.dst_token,
            &MIN_DST,
            &None,  // deadline — use contract default (INTENT_EXPIRY)
            &None,  // referrer
        )
    }

    /// Mint `fill_amount + fee` dst tokens to the solver so `begin_fill` /
    /// `fill_intent` can transfer them to the user and fee_recipient.
    fn fund_solver_for_fill(&self, fill_amount: i128) {
        let fee = fill_amount * FEE_BPS / 10_000;
        self.dst_admin().mint(&self.solver, &(fill_amount + fee));
    }
}

fn setup() -> Ctx {
    let env = Env::default();
    env.mock_all_auths();

    let admin          = Address::generate(&env);
    let fee_recipient  = Address::generate(&env);
    let user           = Address::generate(&env);
    let solver         = Address::generate(&env);

    let bond_token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let dst_token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let contract_id = env.register_contract(None, IntentSettlement);

    let ctx = Ctx { env, admin, fee_recipient, user, solver, contract_id, bond_token, dst_token };

    ctx.client().initialize(&ctx.admin, &ctx.fee_recipient, &ctx.bond_token);
    ctx
}

// ── Trace 1: Happy path (Open → Accepted → Filled) ───────────────────────────
// Quint trace: happyPath
// INV-4: Accepted intent has solver assigned.
// INV-5: Filled intent fill_amount ≥ min_dst_amount.
#[test]
fn conformance_happy_path() {
    let ctx = setup();
    ctx.register_solver();

    let intent_id = ctx.submit();
    ctx.fund_solver_for_fill(FILL);

    let c = ctx.client();

    c.accept_intent(&ctx.solver, &intent_id);

    // INV-4 check
    let rec = c.get_intent(&intent_id).expect("intent must exist");
    assert_eq!(rec.state, IntentState::Accepted, "state must be Accepted");
    assert!(rec.solver.is_some(), "INV-4: Accepted intent must have solver assigned");

    c.fill_intent(&ctx.solver, &intent_id, &FILL, &false);

    let rec = c.get_intent(&intent_id).expect("intent must exist");
    assert_eq!(rec.state, IntentState::Filled, "state must be Filled");
    // INV-5 check
    let fill_amt = rec.fill_amount.expect("Filled intent must record fill_amount");
    assert!(
        fill_amt >= rec.min_dst_amount,
        "INV-5: fill_amount ({fill_amt}) must be >= min_dst_amount ({})",
        rec.min_dst_amount
    );
}

// ── Trace 2: Cancel path (Open → Cancelled) ──────────────────────────────────
// Quint trace: cancelPath
#[test]
fn conformance_cancel_path() {
    let ctx = setup();
    let intent_id = ctx.submit();

    ctx.client().cancel_intent(&ctx.user, &intent_id);

    let rec = ctx.client().get_intent(&intent_id).expect("intent must exist");
    assert_eq!(rec.state, IntentState::Cancelled, "state must be Cancelled");
}

// ── Trace 3: Expire path (Open → Expired) ────────────────────────────────────
// Quint trace: expirePath
#[test]
fn conformance_expire_path() {
    let ctx = setup();
    let intent_id = ctx.submit();

    // INTENT_EXPIRY default = 1800 seconds; advance past it
    ctx.pass_time(1_801);

    ctx.client().expire_intent(&intent_id);

    let rec = ctx.client().get_intent(&intent_id).expect("intent must exist");
    assert_eq!(rec.state, IntentState::Expired, "state must be Expired");
}

// ── Trace 4: Slash path (Accepted → Open, bond slashed) ──────────────────────
// Quint trace: slashPath
// INV-2: solver bond must not go negative after slash.
#[test]
fn conformance_slash_path() {
    let ctx = setup();
    ctx.register_solver();

    let intent_id = ctx.submit();

    let c = ctx.client();
    c.accept_intent(&ctx.solver, &intent_id);

    let bond_before = c.get_solver(&ctx.solver)
        .expect("solver must exist")
        .bond_amount;

    // FILL_WINDOW default = 300 seconds; advance past it
    ctx.pass_time(301);

    c.slash_solver(&intent_id);

    // Trace postconditions: re-opened, solver cleared
    let rec = c.get_intent(&intent_id).expect("intent must exist");
    assert_eq!(rec.state, IntentState::Open, "slashed intent must be re-opened");
    assert!(rec.solver.is_none(), "re-opened intent must have no solver");

    // INV-2: bond reduced but never negative
    let bond_after = c.get_solver(&ctx.solver)
        .expect("solver must still exist after slash")
        .bond_amount;
    assert!(bond_after < bond_before, "slash must reduce solver bond");
    assert!(bond_after >= 0, "INV-2: solver bond must never be negative");
}

// ── Trace 5: Dispute upheld (Filling → Disputed → Resolved + slash) ──────────
// Quint trace: disputeUpheldPath
// INV-2: bond ≥ 0 after slash.  INV-8: Resolved carries an outcome.
#[test]
fn conformance_dispute_upheld() {
    let ctx = setup();
    ctx.register_solver();

    // Also mint a DISPUTE_BOND worth of bond_token to the user (contract
    // requires an anti-griefing bond from the disputing user).
    ctx.bond_admin().mint(&ctx.user, &10_000_000i128); // 1 USDC

    let intent_id = ctx.submit();
    ctx.fund_solver_for_fill(FILL);

    let c = ctx.client();
    c.accept_intent(&ctx.solver, &intent_id);

    let bond_before = c.get_solver(&ctx.solver)
        .expect("solver must exist")
        .bond_amount;

    // Solver commits fill to escrow; begin_fill requires fill_amount param
    c.begin_fill(&ctx.solver, &intent_id, &FILL);

    // User opens dispute within the dispute window
    c.open_dispute(&ctx.user, &intent_id);

    let rec = c.get_intent(&intent_id).expect("intent must exist");
    assert_eq!(rec.state, IntentState::Disputed, "state must be Disputed");

    // Arbiter (= admin in v1) resolves in user's favour
    c.resolve_dispute(&ctx.admin, &intent_id, &DisputeResolution::Upheld);

    let rec = c.get_intent(&intent_id).expect("intent must exist");
    assert_eq!(rec.state, IntentState::Resolved, "state must be Resolved");
    // INV-8
    assert!(rec.resolution.is_some(), "INV-8: Resolved intent must carry a resolution");

    let bond_after = c.get_solver(&ctx.solver)
        .expect("solver must exist")
        .bond_amount;
    assert!(bond_after < bond_before, "Upheld dispute must slash solver bond");
    assert!(bond_after >= 0, "INV-2: solver bond must never be negative");
}

// ── Trace 6: Dispute dismissed (Filling → Disputed → Resolved, no slash) ─────
// Quint trace: disputeDismissedPath
// INV-8: Resolved carries an outcome.  No slash on Dismissed.
#[test]
fn conformance_dispute_dismissed() {
    let ctx = setup();
    ctx.register_solver();

    ctx.bond_admin().mint(&ctx.user, &10_000_000i128); // dispute bond

    let intent_id = ctx.submit();
    ctx.fund_solver_for_fill(FILL);

    let c = ctx.client();
    c.accept_intent(&ctx.solver, &intent_id);

    let bond_before = c.get_solver(&ctx.solver)
        .expect("solver must exist")
        .bond_amount;

    c.begin_fill(&ctx.solver, &intent_id, &FILL);
    c.open_dispute(&ctx.user, &intent_id);

    // Arbiter rules for the solver — no slash
    c.resolve_dispute(&ctx.admin, &intent_id, &DisputeResolution::Dismissed);

    let rec = c.get_intent(&intent_id).expect("intent must exist");
    assert_eq!(rec.state, IntentState::Resolved, "state must be Resolved");
    assert!(rec.resolution.is_some(), "INV-8: Resolved intent must carry a resolution");

    let bond_after = c.get_solver(&ctx.solver)
        .expect("solver must exist")
        .bond_amount;
    assert_eq!(bond_after, bond_before, "Dismissed dispute must NOT slash solver bond");
}
