//! # Fee Router
//!
//! A pull-based fee router that accumulates protocol revenue per token and
//! distributes it to governance-configured sinks according to timelocked
//! weights (in basis points, summing to 10,000).
//!
//! Design notes:
//! - Accounting is pull-based: `distribute` credits `claimable[sink][token]`
//!   and sinks withdraw with `claim`. A sink that rejects a transfer does not
//!   revert the whole distribution; its share stays claimable for a later retry.
//! - Weight changes are timelocked: `propose_weights` then `apply_weights`
//!   after `weight_delay` has elapsed.
//! - Rounding dust from integer division is assigned to the first sink.
//! - Fee-on-transfer tokens are supported: the router credits the amount it
//!   actually received, not the amount the caller claimed to send.

use std::collections::BTreeMap;

/// Total basis points that weights must sum to.
pub const TOTAL_BPS: u32 = 10_000;

/// Maximum number of sinks the router will accept.
pub const MAX_SINKS: usize = 16;

/// A single revenue sink with its governance-set weight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sink {
    pub account: String,
    pub weight_bps: u32,
}

/// A pending, timelocked weight change.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingWeights {
    sinks: Vec<Sink>,
    executable_at: u64,
}

/// Errors returned by the router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeeRouterError {
    Unauthorized,
    TooManySinks,
    WeightsMustSumToTotal,
    EmptySinks,
    DuplicateSink,
    NoPendingWeights,
    TimelockNotElapsed,
    UnknownToken,
    NothingToClaim,
    TransferFailed,
}

/// The fee router contract.
#[derive(Debug)]
pub struct FeeRouter {
    governance: String,
    weight_delay: u64,
    sinks: Vec<Sink>,
    pending: Option<PendingWeights>,
    /// token -> total accumulated but not yet distributed balance.
    balances: BTreeMap<String, u128>,
    /// sink -> token -> claimable amount.
    claimable: BTreeMap<String, BTreeMap<String, u128>>,
}

impl FeeRouter {
    /// Create a new router. `sinks` must be non-empty, unique, at most
    /// [`MAX_SINKS`], and sum to [`TOTAL_BPS`].
    pub fn new(
        governance: impl Into<String>,
        weight_delay: u64,
        sinks: Vec<Sink>,
    ) -> Result<Self, FeeRouterError> {
        validate_sinks(&sinks)?;
        Ok(Self {
            governance: governance.into(),
            weight_delay,
            sinks,
            pending: None,
            balances: BTreeMap::new(),
            claimable: BTreeMap::new(),
        })
    }

    pub fn governance(&self) -> &str {
        &self.governance
    }

    pub fn weight_delay(&self) -> u64 {
        self.weight_delay
    }

    pub fn sinks(&self) -> &[Sink] {
        &self.sinks
    }

    /// Total accumulated balance for `token` that has not yet been distributed.
    pub fn balance_of(&self, token: &str) -> u128 {
        self.balances.get(token).copied().unwrap_or(0)
    }

    /// Amount currently claimable by `sink` for `token`.
    pub fn claimable(&self, sink: &str, token: &str) -> u128 {
        self.claimable
            .get(sink)
            .and_then(|m| m.get(token))
            .copied()
            .unwrap_or(0)
    }

    /// Record an incoming fee payment. The router credits the amount it
    /// actually received, which makes fee-on-transfer tokens safe.
    pub fn on_fee_received(&mut self, token: impl Into<String>, amount: u128) {
        if amount == 0 {
            return;
        }
        let entry = self.balances.entry(token.into()).or_insert(0);
        *entry = entry.saturating_add(amount);
    }

    /// Permissionlessly distribute the accumulated balance of `token` across
    /// the configured sinks according to their weights.
    ///
    /// Rounding dust is assigned to the first sink so that the full balance is
    /// always conserved.
    pub fn distribute(&mut self, token: &str) -> Result<(), FeeRouterError> {
        let total = self.balance_of(token);
        if total == 0 {
            return Err(FeeRouterError::UnknownToken);
        }

        let mut distributed: u128 = 0;
        for (i, sink) in self.sinks.iter().enumerate() {
            let share = if i == 0 {
                // First sink absorbs rounding dust.
                total - distributed
            } else {
                total.saturating_mul(sink.weight_bps as u128) / TOTAL_BPS as u128
            };
            distributed = distributed.saturating_add(share);
            if share > 0 {
                let entry = self
                    .claimable
                    .entry(sink.account.clone())
                    .or_default()
                    .entry(token.to_string())
                    .or_insert(0);
                *entry = entry.saturating_add(share);
            }
        }

        self.balances.insert(token.to_string(), 0);
        Ok(())
    }

    /// Withdraw the caller's claimable balance for `token`.
    ///
    /// The transfer is performed by the caller-supplied closure. If the sink
    /// rejects the transfer the share is left claimable and the error is
    /// surfaced, so a later retry can succeed without affecting other sinks.
    pub fn claim<F>(&mut self, sink: &str, token: &str, transfer: F) -> Result<u128, FeeRouterError>
    where
        F: FnOnce(&str, &str, u128) -> bool,
    {
        let amount = self.claimable(sink, token);
        if amount == 0 {
            return Err(FeeRouterError::NothingToClaim);
        }
        if !transfer(sink, token, amount) {
            return Err(FeeRouterError::TransferFailed);
        }
        if let Some(m) = self.claimable.get_mut(sink) {
            m.insert(token.to_string(), 0);
        }
        Ok(amount)
    }

    /// Propose a new set of sinks and weights. Only governance may call this.
    pub fn propose_weights(
        &mut self,
        caller: &str,
        sinks: Vec<Sink>,
        now: u64,
    ) -> Result<(), FeeRouterError> {
        if caller != self.governance {
            return Err(FeeRouterError::Unauthorized);
        }
        validate_sinks(&sinks)?;
        self.pending = Some(PendingWeights {
            sinks,
            executable_at: now.saturating_add(self.weight_delay),
        });
        Ok(())
    }

    /// Apply a previously proposed weight change once the timelock has elapsed.
    pub fn apply_weights(&mut self, caller: &str, now: u64) -> Result<(), FeeRouterError> {
        if caller != self.governance {
            return Err(FeeRouterError::Unauthorized);
        }
        let pending = self
            .pending
            .take()
            .ok_or(FeeRouterError::NoPendingWeights)?;
        if now < pending.executable_at {
            self.pending = Some(pending);
            return Err(FeeRouterError::TimelockNotElapsed);
        }
        self.sinks = pending.sinks;
        Ok(())
    }

    /// Timestamp at which the pending weight change becomes executable.
    pub fn pending_executable_at(&self) -> Option<u64> {
        self.pending.as_ref().map(|p| p.executable_at)
    }
}

fn validate_sinks(sinks: &[Sink]) -> Result<(), FeeRouterError> {
    if sinks.is_empty() {
        return Err(FeeRouterError::EmptySinks);
    }
    if sinks.len() > MAX_SINKS {
        return Err(FeeRouterError::TooManySinks);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut sum: u32 = 0;
    for sink in sinks {
        if !seen.insert(sink.account.as_str()) {
            return Err(FeeRouterError::DuplicateSink);
        }
        sum = sum.saturating_add(sink.weight_bps);
    }
    if sum != TOTAL_BPS {
        return Err(FeeRouterError::WeightsMustSumToTotal);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sinks() -> Vec<Sink> {
        vec![
            Sink { account: "treasury".into(), weight_bps: 5_000 },
            Sink { account: "backstop".into(), weight_bps: 3_000 },
            Sink { account: "rebate".into(), weight_bps: 2_000 },
        ]
    }

    #[test]
    fn distribute_conserves_balance() {
        let mut r = FeeRouter::new("gov", 100, sinks()).unwrap();
        r.on_fee_received("usd", 10_001);
        r.distribute("usd").unwrap();
        let total = r.claimable("treasury", "usd")
            + r.claimable("backstop", "usd")
            + r.claimable("rebate", "usd");
        assert_eq!(total, 10_001);
        assert_eq!(r.balance_of("usd"), 0);
    }

    #[test]
    fn weights_must_sum_to_total() {
        let bad = vec![Sink { account: "a".into(), weight_bps: 9_999 }];
        assert_eq!(FeeRouter::new("gov", 0, bad).unwrap_err(), FeeRouterError::WeightsMustSumToTotal);
    }

    #[test]
    fn timelock_enforced() {
        let mut r = FeeRouter::new("gov", 100, sinks()).unwrap();
        r.propose_weights("gov", vec![Sink { account: "a".into(), weight_bps: 10_000 }], 0).unwrap();
        assert_eq!(r.apply_weights("gov", 50).unwrap_err(), FeeRouterError::TimelockNotElapsed);
        r.apply_weights("gov", 100).unwrap();
        assert_eq!(r.sinks().len(), 1);
    }

    #[test]
    fn rejected_transfer_keeps_share_claimable() {
        let mut r = FeeRouter::new("gov", 0, sinks()).unwrap();
        r.on_fee_received("usd", 1_000);
        r.distribute("usd").unwrap();
        let before = r.claimable("treasury", "usd");
        assert_eq!(r.claim("treasury", "usd", |_, _, _| false).unwrap_err(), FeeRouterError::TransferFailed);
        assert_eq!(r.claimable("treasury", "usd"), before);
        assert_eq!(r.claim("treasury", "usd", |_, _, _| true).unwrap(), before);
    }
}
