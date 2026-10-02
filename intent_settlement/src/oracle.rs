//! Oracle adapter module for Vortex Protocol (#368).
//!
//! Provides an `OracleAdapter` client interface compatible with SEP-40
//! (Reflector protocol). The settlement contract queries asset prices
//! through this adapter for exposure checks and min-bond adjustments.
//!
//! ## Staleness guard
//! Prices older than `MAX_PRICE_AGE_SECS` are rejected (fail-closed).
//!
//! ## Deviation guard
//! If two prices disagree by more than `MAX_PRICE_DEVIATION_BPS` the
//! data is treated as unreliable (fail-closed).
//!
//! ## Fail-closed behaviour
//! On stale / zero / missing price data:
//! - `accept_intent` exposure check → uses conservative 1.0× weight
//!   (the check still runs, so an over-exposed solver is still blocked)
//! - `slash_solver` → proceeds regardless (slashing must never be blocked)

#![allow(unused)]

use soroban_sdk::{contractclient, Address, Env};

// ─── SEP-40 compatible oracle client ─────────────────────────────────────────

/// SEP-40 `lastprice` result.
#[soroban_sdk::contracttype]
#[derive(Clone, Debug)]
pub struct PriceData {
    /// Price in oracle-defined units, normalised by `decimals`.
    pub price: i128,
    /// Decimal precision of `price`.
    pub decimals: u32,
    /// Unix timestamp of the observation (seconds).
    pub timestamp: u64,
}

/// Minimal SEP-40 oracle interface.
/// The settlement contract expects a deployed contract that implements at
/// least `lastprice(asset: Address) -> Option<PriceData>`.
#[contractclient(name = "OracleAdapterClient")]
pub trait OracleAdapterTrait {
    /// Return the latest price for `asset`, or `None` if unavailable.
    fn lastprice(env: Env, asset: Address) -> Option<PriceData>;
}

// ─── Constants ────────────────────────────────────────────────────────────────

/// Maximum age of a valid price in seconds (5 minutes).
pub const MAX_PRICE_AGE_SECS: u64 = 300;

/// Maximum allowed deviation between two oracle sources, in basis points (2%).
pub const MAX_PRICE_DEVIATION_BPS: i128 = 200;

// ─── Result type ─────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub enum OracleFault {
    /// Oracle returned `None` — no price available.
    NotAvailable,
    /// Price is older than `MAX_PRICE_AGE_SECS`.
    Stale,
    /// Price is zero or negative — unusable.
    ZeroOrNegative,
}

// ─── Public helpers ───────────────────────────────────────────────────────────

/// Fetch the price of `asset` from the oracle at `oracle_addr`.
///
/// Returns `Ok((price, decimals))` on success.
/// Returns `Err(OracleFault)` if the price is stale, zero, or unavailable.
pub fn fetch_price(
    env: &Env,
    oracle_addr: &Address,
    asset: &Address,
) -> Result<(i128, u32), OracleFault> {
    let client = OracleAdapterClient::new(env, oracle_addr);
    match client.lastprice(asset.clone()) {
        None => Err(OracleFault::NotAvailable),
        Some(pd) => {
            let now = env.ledger().timestamp();
            if now > pd.timestamp && now - pd.timestamp > MAX_PRICE_AGE_SECS {
                return Err(OracleFault::Stale);
            }
            if pd.price <= 0 {
                return Err(OracleFault::ZeroOrNegative);
            }
            Ok((pd.price, pd.decimals))
        }
    }
}

/// Check that two prices (from different sources) agree within
/// `MAX_PRICE_DEVIATION_BPS`. Returns `true` if they agree, `false`
/// if the deviation exceeds the threshold or either price is zero.
pub fn prices_agree(price_a: i128, dec_a: u32, price_b: i128, dec_b: u32) -> bool {
    if price_a <= 0 || price_b <= 0 {
        return false;
    }
    // Normalise to the higher precision.
    let (a, b) = if dec_a >= dec_b {
        let scale = 10i128.pow(dec_a - dec_b);
        (price_a, price_b.saturating_mul(scale))
    } else {
        let scale = 10i128.pow(dec_b - dec_a);
        (price_a.saturating_mul(scale), price_b)
    };
    let diff = (a - b).abs();
    let denom = a.max(b);
    // diff / denom <= MAX_PRICE_DEVIATION_BPS / 10_000
    diff.saturating_mul(10_000) <= MAX_PRICE_DEVIATION_BPS.saturating_mul(denom)
}
