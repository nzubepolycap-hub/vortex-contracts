//! Pure arithmetic helpers — Issue #415
//!
//! This module extracts every arithmetic computation that does **not** require
//! an `Env` reference into standalone `pub(crate)` functions. These functions
//! use only primitive types (`i128`, `u64`, `u32`) so they can be:
//!
//! 1. **Unit-tested** without spinning up a Soroban environment.
//! 2. **Proven** with the [Kani model checker](https://model-checking.github.io/kani/)
//!    — see `intent_settlement/src/kani_proofs.rs` for the harnesses.
//!
//! The helpers are deliberately minimal and allocation-free; they carry no
//! `soroban-sdk` imports.

// ── Protocol constants (mirrored from lib.rs; kept in sync manually) ──────────

/// Basis-points denominator (10 000 = 100%).
pub const BPS_DENOMINATOR: i128 = 10_000;

/// Slash rate in basis points applied by `compute_slash_amount` (10%).
pub const SLASH_BPS: i128 = 1_000;

/// Per-tier fill-window bonus table (bps).
/// Index corresponds to tier: 0 = Unranked … 4 = Platinum.
pub const TIER_FILL_WINDOW_BONUS_BPS: [u64; 5] = [0, 1_000, 2_000, 3_000, 5_000];

/// Per-tier slash rate table (bps of bond).
pub const TIER_SLASH_BPS: [i128; 5] = [1_000, 1_000, 800, 600, 500];

/// Minimum slash rate (Platinum tier floor).
pub const MIN_SLASH_BPS: i128 = 500;

/// Maximum allowed protocol fee in basis points (10%).
pub const MAX_PROTOCOL_FEE_BPS: i128 = 1_000;

// ── Slash arithmetic ──────────────────────────────────────────────────────────

/// Compute the amount to slash from `bond` for a solver that failed to deliver
/// an intent with `unfilled_amount` outstanding output tokens.
///
/// Formula:
/// ```text
/// exposure     = clamp(unfilled_amount, 0, bond)
/// proportional = exposure / 10
/// cap          = bond * SLASH_BPS / BPS_DENOMINATOR   (10% of bond)
/// result       = clamp(proportional, 1, min(cap, bond))
/// ```
///
/// Guaranteed properties (verified by Kani in `kani_proofs.rs`):
/// - Result is **always ≥ 0**.
/// - Result is **always ≤ bond** (bond can never go negative from a single slash).
/// - For `bond > 0` the result is **always ≥ 1** (non-zero bond is always penalised).
/// - No arithmetic overflow for any valid `i128` inputs.
pub fn compute_slash_amount(bond: i128, unfilled_amount: i128) -> i128 {
    if bond <= 0 {
        return 0;
    }
    let exposure = unfilled_amount.max(0).min(bond);
    let proportional = exposure / 10;
    // cap = bond * SLASH_BPS / BPS_DENOMINATOR, computed to avoid overflow:
    // bond is at most i128::MAX; SLASH_BPS = 1_000; BPS_DENOMINATOR = 10_000.
    // Intermediate: bond * 1_000 may overflow for very large bonds, but in
    // practice bonds are bounded by MAX_AMOUNT (10^30), well within i128 range.
    let cap = (bond / BPS_DENOMINATOR) * SLASH_BPS;
    let cap = cap.min(bond).max(1);
    proportional.max(1).min(cap)
}

/// Compute the slash amount using a tier-specific slash rate instead of the
/// flat `SLASH_BPS` constant.
///
/// `tier` is clamped to the `TIER_SLASH_BPS` table length before lookup.
pub fn compute_slash_amount_tiered(bond: i128, unfilled_amount: i128, tier: u32) -> i128 {
    if bond <= 0 {
        return 0;
    }
    let idx = (tier as usize).min(TIER_SLASH_BPS.len() - 1);
    let slash_bps = TIER_SLASH_BPS[idx];
    let exposure = unfilled_amount.max(0).min(bond);
    let proportional = exposure / 10;
    let cap = (bond / BPS_DENOMINATOR) * slash_bps;
    let cap = cap.min(bond).max(1);
    proportional.max(1).min(cap)
}

// ── Fill-window arithmetic ────────────────────────────────────────────────────

/// Compute the fill window (in seconds) granted to a solver at `tier`.
///
/// Formula:
/// ```text
/// bonus_bps   = TIER_FILL_WINDOW_BONUS_BPS[tier]     (0 for Unranked)
/// fill_window = base_fill_window * (10_000 + bonus_bps) / 10_000
/// ```
///
/// Uses `saturating_mul` to prevent overflow on extreme inputs; the result is
/// always ≥ `base_fill_window` (bonuses are non-negative).
pub fn tier_fill_window(tier: u32, base_fill_window: u64) -> u64 {
    let idx = (tier as usize).min(TIER_FILL_WINDOW_BONUS_BPS.len() - 1);
    let bonus_bps = TIER_FILL_WINDOW_BONUS_BPS[idx];
    base_fill_window.saturating_mul(10_000 + bonus_bps) / 10_000
}

// ── Fee arithmetic ────────────────────────────────────────────────────────────

/// Compute the protocol fee charged on a fill of `fill_amount` tokens.
///
/// Formula:
/// ```text
/// fee = fill_amount * effective_fee_bps / BPS_DENOMINATOR
/// ```
///
/// Returns `None` if the multiplication overflows `i128` (this can only happen
/// for `fill_amount` near `i128::MAX`, which the contract rejects via
/// `MAX_AMOUNT` before reaching this function).
///
/// Guaranteed properties (verified by Kani):
/// - `fee ≤ fill_amount` for any `effective_fee_bps ≤ BPS_DENOMINATOR`.
/// - Fee rounds **down** (truncating division; solver never overpays).
/// - For `effective_fee_bps == 0`, `fee == 0`.
pub fn compute_fee(fill_amount: i128, effective_fee_bps: i128) -> Option<i128> {
    if effective_fee_bps < 0 || effective_fee_bps > BPS_DENOMINATOR {
        return None;
    }
    fill_amount.checked_mul(effective_fee_bps).map(|p| p / BPS_DENOMINATOR)
}

/// Compute the effective fee basis points after applying a volume-tier discount.
///
/// ```text
/// discount   = min(discount_bps, BPS_DENOMINATOR)    // cap at 100%
/// reduction  = base_fee_bps * discount / BPS_DENOMINATOR
/// effective  = max(base_fee_bps - reduction, 0)
/// ```
///
/// Guaranteed: result is in `0 ..= base_fee_bps` (discount can never make the
/// fee larger than the un-discounted rate or go negative).
pub fn apply_discount(base_fee_bps: i128, discount_bps: i128) -> i128 {
    if base_fee_bps <= 0 {
        return 0;
    }
    let capped_discount = discount_bps.min(BPS_DENOMINATOR).max(0);
    let reduction = base_fee_bps * capped_discount / BPS_DENOMINATOR;
    (base_fee_bps - reduction).max(0)
}

// ── Dutch-auction decay arithmetic ───────────────────────────────────────────

/// Compute the current minimum destination amount for a Dutch-auction intent.
///
/// The price decays linearly from `start_dst_amount` to `min_dst_amount` over
/// the interval `[decay_start, decay_end]`.
///
/// ```text
/// if now <= decay_start : start_dst_amount
/// if now >= decay_end   : min_dst_amount
/// else                  : start - (start - min) * (now - decay_start) / (decay_end - decay_start)
/// ```
///
/// Returns `min_dst_amount` when any optional parameter is `None` (non-auction
/// intent), matching the contract's `current_min_dst` fallback arm.
///
/// Guaranteed properties (verified by Kani):
/// - Result is always in `[min_dst_amount, start_dst_amount]` when
///   `start_dst_amount >= min_dst_amount`.
/// - No division by zero (guarded by `decay_end > decay_start` precondition).
pub fn dutch_decay(
    now: u64,
    start_dst_amount: Option<i128>,
    min_dst_amount: i128,
    decay_start: Option<u64>,
    decay_end: Option<u64>,
) -> i128 {
    match (start_dst_amount, decay_start, decay_end) {
        (Some(start), Some(ds), Some(de)) if de > ds && start >= min_dst_amount => {
            if now >= de {
                min_dst_amount
            } else if now <= ds {
                start
            } else {
                let elapsed = now - ds;
                let total_duration = de - ds;
                let decay_amount = start - min_dst_amount;
                start - (decay_amount * elapsed as i128) / total_duration as i128
            }
        }
        _ => min_dst_amount,
    }
}

// ── Tier lookup ───────────────────────────────────────────────────────────────

/// Look up the slash rate (in bps) for `tier`, clamped to the table size.
///
/// Guaranteed: result is in `[MIN_SLASH_BPS, TIER_SLASH_BPS[0]]` for any `tier`.
pub fn slash_bps_for_tier(tier: u32) -> i128 {
    let idx = (tier as usize).min(TIER_SLASH_BPS.len() - 1);
    TIER_SLASH_BPS[idx]
}

/// Look up the fill-window bonus (in bps) for `tier`, clamped to the table size.
///
/// Guaranteed: result is in `[0, TIER_FILL_WINDOW_BONUS_BPS[4]]` for any `tier`.
pub fn fill_window_bonus_bps_for_tier(tier: u32) -> u64 {
    let idx = (tier as usize).min(TIER_FILL_WINDOW_BONUS_BPS.len() - 1);
    TIER_FILL_WINDOW_BONUS_BPS[idx]
}

// ── Payload byte decoding (proof_registry) ────────────────────────────────────

/// Decode a big-endian `i128` from 16 contiguous bytes starting at `offset`
/// within a fixed-size byte slice.
///
/// Returns `None` if `offset + 16 > slice.len()`.
///
/// This is the pure equivalent of the Soroban-SDK-specific loop in
/// `proof_registry::receive_message`.
pub fn decode_i128_be(bytes: &[u8], offset: usize) -> Option<i128> {
    if offset.checked_add(16)? > bytes.len() {
        return None;
    }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&bytes[offset..offset + 16]);
    Some(i128::from_be_bytes(arr))
}

/// Decode a big-endian `u32` from 4 contiguous bytes starting at `offset`.
///
/// Returns `None` if `offset + 4 > slice.len()`.
pub fn decode_u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    if offset.checked_add(4)? > bytes.len() {
        return None;
    }
    let mut arr = [0u8; 4];
    arr.copy_from_slice(&bytes[offset..offset + 4]);
    Some(u32::from_be_bytes(arr))
}

/// Decode a big-endian `u16` from 2 contiguous bytes starting at `offset`.
///
/// Returns `None` if `offset + 2 > slice.len()`.
pub fn decode_u16_be(bytes: &[u8], offset: usize) -> Option<u16> {
    if offset.checked_add(2)? > bytes.len() {
        return None;
    }
    let mut arr = [0u8; 2];
    arr.copy_from_slice(&bytes[offset..offset + 2]);
    Some(u16::from_be_bytes(arr))
}

/// Extract the `src_chain_id` from the fixed 102-byte proof payload.
///
/// Layout:
/// ```text
/// [0..32]  intent_id
/// [32..52] src_user (EVM address, 20 bytes)
/// [52..54] src_chain_id (u16, big-endian)
/// [54..86] src_token (32 bytes)
/// [86..102] src_amount (i128, big-endian)
/// ```
pub fn decode_payload_chain_id(payload: &[u8]) -> Option<u16> {
    if payload.len() != 102 {
        return None;
    }
    decode_u16_be(payload, 52)
}

/// Extract the `src_amount` from the fixed 102-byte proof payload.
pub fn decode_payload_src_amount(payload: &[u8]) -> Option<i128> {
    if payload.len() != 102 {
        return None;
    }
    decode_i128_be(payload, 86)
}

// ── Unit tests (no Soroban env required) ──────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_slash_amount ──────────────────────────────────────────────────

    #[test]
    fn slash_zero_bond_returns_zero() {
        assert_eq!(compute_slash_amount(0, 100), 0);
    }

    #[test]
    fn slash_negative_bond_returns_zero() {
        assert_eq!(compute_slash_amount(-1, 100), 0);
    }

    #[test]
    fn slash_floor_one_when_bond_nonzero() {
        // Very small bond: 1 stroop. Proportional = 0, so floor kicks in → 1.
        assert_eq!(compute_slash_amount(1, 1), 1);
    }

    #[test]
    fn slash_never_exceeds_bond() {
        let bond = 50 * 10_000_000i128; // 50 USDC
        let result = compute_slash_amount(bond, bond * 2);
        assert!(result <= bond, "slash must not exceed bond");
        assert!(result >= 0, "slash must be non-negative");
    }

    #[test]
    fn slash_proportional_formula() {
        // bond = 1_000, unfilled = 1_000
        // exposure = 1_000, proportional = 100
        // cap = (1_000 / 10_000) * 1_000 = 100  → result = 100
        assert_eq!(compute_slash_amount(1_000, 1_000), 100);
    }

    #[test]
    fn slash_zero_unfilled_still_floors_at_one() {
        let bond = 500_000_000i128; // 50 USDC
        let result = compute_slash_amount(bond, 0);
        assert!(result >= 1, "must slash at least 1 stroop even on zero unfilled");
        assert!(result <= bond);
    }

    // ── tier_fill_window ──────────────────────────────────────────────────────

    #[test]
    fn fill_window_unranked_is_base() {
        assert_eq!(tier_fill_window(0, 300), 300);
    }

    #[test]
    fn fill_window_increases_monotonically() {
        let base = 300u64;
        for tier in 0..4 {
            assert!(
                tier_fill_window(tier + 1, base) >= tier_fill_window(tier, base),
                "fill window must be monotonically non-decreasing with tier"
            );
        }
    }

    #[test]
    fn fill_window_no_overflow_on_large_base() {
        // u64::MAX / 2 base; saturating_mul should prevent overflow
        let large_base = u64::MAX / 2;
        let _ = tier_fill_window(4, large_base); // must not panic
    }

    #[test]
    fn fill_window_out_of_range_tier_clamped() {
        // tier 99 should clamp to tier 4 (Platinum)
        assert_eq!(tier_fill_window(99, 300), tier_fill_window(4, 300));
    }

    // ── compute_fee ──────────────────────────────────────────────────────────

    #[test]
    fn fee_zero_bps_is_zero() {
        assert_eq!(compute_fee(1_000_000, 0), Some(0));
    }

    #[test]
    fn fee_does_not_exceed_amount() {
        let amount: i128 = 1_000_000_000;
        let fee = compute_fee(amount, MAX_PROTOCOL_FEE_BPS).unwrap();
        assert!(fee <= amount, "fee must never exceed fill amount");
    }

    #[test]
    fn fee_rounds_down() {
        // 1 stroop * 5 bps / 10_000 = 0 (truncating)
        assert_eq!(compute_fee(1, 5), Some(0));
    }

    #[test]
    fn fee_rejects_negative_bps() {
        assert_eq!(compute_fee(1_000, -1), None);
    }

    #[test]
    fn fee_rejects_bps_above_denominator() {
        assert_eq!(compute_fee(1_000, BPS_DENOMINATOR + 1), None);
    }

    #[test]
    fn fee_at_5bps_matches_proptest_formula() {
        let fill: i128 = 35_000_000;
        let expected = fill * 5 / 10_000;
        assert_eq!(compute_fee(fill, 5), Some(expected));
    }

    // ── apply_discount ────────────────────────────────────────────────────────

    #[test]
    fn discount_zero_leaves_fee_unchanged() {
        assert_eq!(apply_discount(100, 0), 100);
    }

    #[test]
    fn discount_full_waives_fee() {
        assert_eq!(apply_discount(100, BPS_DENOMINATOR), 0);
    }

    #[test]
    fn discount_never_increases_fee() {
        for discount in [0, 1000, 5000, 9999, 10000, 20000] {
            let result = apply_discount(500, discount);
            assert!(result <= 500, "discount must not increase the fee");
        }
    }

    #[test]
    fn discount_never_goes_negative() {
        assert!(apply_discount(5, 20_000) >= 0);
    }

    // ── dutch_decay ──────────────────────────────────────────────────────────

    #[test]
    fn dutch_decay_before_start_returns_start_amount() {
        let result = dutch_decay(0, Some(100), 10, Some(5), Some(20));
        assert_eq!(result, 100);
    }

    #[test]
    fn dutch_decay_after_end_returns_min() {
        let result = dutch_decay(25, Some(100), 10, Some(5), Some(20));
        assert_eq!(result, 10);
    }

    #[test]
    fn dutch_decay_midway_is_between_bounds() {
        let result = dutch_decay(12, Some(100), 10, Some(10), Some(20));
        // at 12, elapsed=2, total=10, range=90: result = 100 - 90*2/10 = 82
        assert_eq!(result, 82);
        assert!(result >= 10 && result <= 100);
    }

    #[test]
    fn dutch_decay_none_params_returns_min() {
        assert_eq!(dutch_decay(5, None, 42, None, None), 42);
    }

    #[test]
    fn dutch_decay_result_always_in_bounds() {
        for now in 0u64..=25 {
            let result = dutch_decay(now, Some(100), 10, Some(5), Some(20));
            assert!(
                result >= 10 && result <= 100,
                "dutch result {result} out of [min, start] at now={now}"
            );
        }
    }

    // ── tier lookup ──────────────────────────────────────────────────────────

    #[test]
    fn slash_bps_clamped_for_out_of_range_tier() {
        let clamped = slash_bps_for_tier(999);
        let max_tier = slash_bps_for_tier(4);
        assert_eq!(clamped, max_tier);
    }

    #[test]
    fn slash_bps_monotonically_non_increasing() {
        // Higher tiers get lower (or equal) slash rates.
        for tier in 0..4 {
            assert!(
                slash_bps_for_tier(tier) >= slash_bps_for_tier(tier + 1),
                "slash rate must not increase with tier"
            );
        }
    }

    // ── decode helpers ────────────────────────────────────────────────────────

    #[test]
    fn decode_i128_roundtrip() {
        let val: i128 = 1_000_000_000_000_000i128;
        let mut buf = vec![0u8; 16];
        buf[..16].copy_from_slice(&val.to_be_bytes());
        assert_eq!(decode_i128_be(&buf, 0), Some(val));
    }

    #[test]
    fn decode_i128_negative_roundtrip() {
        let val: i128 = -42;
        let mut buf = vec![0u8; 20];
        buf[4..20].copy_from_slice(&val.to_be_bytes());
        assert_eq!(decode_i128_be(&buf, 4), Some(val));
    }

    #[test]
    fn decode_i128_out_of_bounds_returns_none() {
        let buf = vec![0u8; 10];
        assert_eq!(decode_i128_be(&buf, 5), None); // 5 + 16 = 21 > 10
    }

    #[test]
    fn decode_payload_chain_id_correct_offset() {
        let mut payload = vec![0u8; 102];
        // Write chain_id = 2 (Ethereum) at bytes [52..54]
        payload[52] = 0x00;
        payload[53] = 0x02;
        assert_eq!(decode_payload_chain_id(&payload), Some(2u16));
    }

    #[test]
    fn decode_payload_wrong_length_returns_none() {
        let payload = vec![0u8; 50]; // wrong length
        assert_eq!(decode_payload_chain_id(&payload), None);
        assert_eq!(decode_payload_src_amount(&payload), None);
    }

    #[test]
    fn decode_payload_src_amount_roundtrip() {
        let amount: i128 = 1_000_000_000_000_000_000i128; // 1 ETH in wei
        let mut payload = vec![0u8; 102];
        payload[86..102].copy_from_slice(&amount.to_be_bytes());
        assert_eq!(decode_payload_src_amount(&payload), Some(amount));
    }
}
