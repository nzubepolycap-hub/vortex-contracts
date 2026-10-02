//! Kani proof harnesses — Issue #415
//!
//! Uses the [Kani model checker](https://model-checking.github.io/kani/) to
//! prove properties of the pure arithmetic helpers in `math.rs` for **all
//! possible inputs**, not just sampled ones.
//!
//! ## Running
//!
//! ```bash
//! # Install Kani (requires Rust toolchain ≥ 1.73):
//! cargo install --locked kani-verifier
//! cargo kani setup
//!
//! # Run all harnesses (≤ 10 minutes total on a standard laptop):
//! make kani
//!
//! # Or run directly:
//! cd intent_settlement && cargo kani --harness kani_slash_never_negative
//! ```
//!
//! ## Design notes
//!
//! - All harnesses use `kani::any::<T>()` to generate fully unconstrained
//!   symbolic inputs; `kani::assume` narrows them to the valid input domain.
//! - `i128` multiplication in `compute_fee` can overflow for inputs near
//!   `i128::MAX`. The contract enforces `MAX_AMOUNT = 10^30` upstream; we add
//!   `kani::assume` to stay in the same range.
//! - Loop unwind bounds (`#[kani::unwind(N)]`) are set conservatively: all
//!   loops in the proven functions are bounded (no iteration over unbounded
//!   collections).

#[cfg(kani)]
mod kani_proofs {
    use crate::math::*;

    // ── HARNESS 1 ──────────────────────────────────────────────────────────────
    /// **Property:** `compute_slash_amount` never returns a negative value.
    ///
    /// For any `bond` and `unfilled_amount`, the slash is always ≥ 0.
    #[kani::proof]
    fn kani_slash_never_negative() {
        let bond: i128 = kani::any();
        let unfilled: i128 = kani::any();
        let result = compute_slash_amount(bond, unfilled);
        kani::assert(result >= 0, "slash amount must never be negative");
    }

    // ── HARNESS 2 ──────────────────────────────────────────────────────────────
    /// **Property:** `compute_slash_amount` never exceeds the bond.
    ///
    /// The slash taken from a solver can never exceed the bond they hold; this
    /// ensures the post-slash bond balance stays ≥ 0.
    #[kani::proof]
    fn kani_slash_never_exceeds_bond() {
        let bond: i128 = kani::any();
        let unfilled: i128 = kani::any();
        kani::assume(bond >= 0);
        let result = compute_slash_amount(bond, unfilled);
        kani::assert(result <= bond, "slash must not exceed bond");
    }

    // ── HARNESS 3 ──────────────────────────────────────────────────────────────
    /// **Property:** A non-zero bond is always penalised by at least 1 stroop.
    ///
    /// The floor-of-1 guard ensures that a solver who has any bond balance is
    /// always penalised, preventing grief-free failures.
    #[kani::proof]
    fn kani_slash_floor_one() {
        let bond: i128 = kani::any();
        let unfilled: i128 = kani::any();
        kani::assume(bond > 0);
        let result = compute_slash_amount(bond, unfilled);
        kani::assert(result >= 1, "non-zero bond must be slashed by at least 1 stroop");
    }

    // ── HARNESS 4 ──────────────────────────────────────────────────────────────
    /// **Property:** `tier_fill_window` is monotonically non-decreasing with tier.
    ///
    /// Higher tiers must receive a fill window ≥ the previous tier's window.
    /// This holds for any base fill window value.
    #[kani::proof]
    #[kani::unwind(6)]
    fn kani_tier_fill_window_monotone() {
        let base: u64 = kani::any();
        kani::assume(base <= u64::MAX / 20_000); // prevent saturating_mul collapse
        for tier in 0u32..4 {
            let lower = tier_fill_window(tier, base);
            let higher = tier_fill_window(tier + 1, base);
            kani::assert(
                higher >= lower,
                "fill window must be non-decreasing with tier",
            );
        }
    }

    // ── HARNESS 5 ──────────────────────────────────────────────────────────────
    /// **Property:** `tier_fill_window` never overflows.
    ///
    /// `saturating_mul` prevents overflow; we verify that the result is always
    /// ≥ base when the multiplication does not saturate.
    #[kani::proof]
    fn kani_tier_fill_window_no_overflow() {
        let tier: u32 = kani::any();
        let base: u64 = kani::any();
        // Constrain to a domain where the multiplication is meaningful
        kani::assume(base <= 1_000_000_000u64); // 1 billion seconds — far beyond realistic
        let result = tier_fill_window(tier, base);
        kani::assert(result >= base, "tier fill window must be >= base fill window");
    }

    // ── HARNESS 6 ──────────────────────────────────────────────────────────────
    /// **Property:** `compute_fee` result is always ≤ `fill_amount`.
    ///
    /// A solver is never charged more in fees than the fill amount itself.
    /// Restricts to valid `fee_bps` range and `fill_amount ≤ MAX_AMOUNT`.
    #[kani::proof]
    fn kani_fee_never_exceeds_amount() {
        let fill: i128 = kani::any();
        let bps: i128 = kani::any();
        // MAX_AMOUNT = 10^30; keep fee multiplication within i128 range
        kani::assume(fill >= 0 && fill <= 1_000_000_000_000_000_000_000_000_000_000i128);
        kani::assume(bps >= 0 && bps <= BPS_DENOMINATOR);
        if let Some(fee) = compute_fee(fill, bps) {
            kani::assert(fee <= fill, "fee must not exceed fill amount");
            kani::assert(fee >= 0, "fee must be non-negative");
        }
    }

    // ── HARNESS 7 ──────────────────────────────────────────────────────────────
    /// **Property:** Fee rounds **down** (truncating integer division).
    ///
    /// For any fill amount, `fee * BPS_DENOMINATOR <= fill_amount * bps`.
    /// This ensures the protocol never over-charges due to rounding.
    #[kani::proof]
    fn kani_fee_rounding_direction() {
        let fill: i128 = kani::any();
        let bps: i128 = kani::any();
        kani::assume(fill >= 0 && fill <= 1_000_000_000_000_000_000_000_000_000_000i128);
        kani::assume(bps >= 0 && bps <= BPS_DENOMINATOR);
        if let Some(fee) = compute_fee(fill, bps) {
            // fee = floor(fill * bps / 10_000)
            // Verify: fee * 10_000 <= fill * bps  (truncation rounds down)
            if let Some(lhs) = fee.checked_mul(BPS_DENOMINATOR) {
                if let Some(rhs) = fill.checked_mul(bps) {
                    kani::assert(lhs <= rhs, "fee must round down");
                }
            }
        }
    }

    // ── HARNESS 8 ──────────────────────────────────────────────────────────────
    /// **Property:** `dutch_decay` result is always in `[min_dst_amount, start]`.
    ///
    /// The Dutch auction price decays monotonically and never falls below the
    /// user's minimum or exceeds the starting price.
    #[kani::proof]
    fn kani_dutch_decay_bounds() {
        let now: u64 = kani::any();
        let start: i128 = kani::any();
        let min_dst: i128 = kani::any();
        let decay_start: u64 = kani::any();
        let decay_end: u64 = kani::any();
        kani::assume(start >= 0 && start >= min_dst && min_dst >= 0);
        kani::assume(decay_end > decay_start);
        kani::assume(decay_start <= u64::MAX - 1);
        let result = dutch_decay(
            now,
            Some(start),
            min_dst,
            Some(decay_start),
            Some(decay_end),
        );
        kani::assert(result >= min_dst, "dutch decay must not go below minimum");
        kani::assert(result <= start, "dutch decay must not exceed starting price");
    }

    // ── HARNESS 9 ──────────────────────────────────────────────────────────────
    /// **Property:** `apply_discount` result is in `[0, base_fee_bps]`.
    ///
    /// A volume-tier discount can reduce the fee but can never make it larger
    /// or negative.
    #[kani::proof]
    fn kani_discount_in_range() {
        let base: i128 = kani::any();
        let discount: i128 = kani::any();
        kani::assume(base >= 0 && base <= BPS_DENOMINATOR);
        let result = apply_discount(base, discount);
        kani::assert(result >= 0, "effective fee must not be negative");
        kani::assert(result <= base, "discount must not increase the fee");
    }

    // ── HARNESS 10 ─────────────────────────────────────────────────────────────
    /// **Property:** `decode_payload_src_amount` round-trips correctly.
    ///
    /// Any `i128` value written as big-endian bytes at offset 86 in a 102-byte
    /// payload is recovered exactly by `decode_payload_src_amount`.
    #[kani::proof]
    fn kani_decode_payload_roundtrip() {
        let amount: i128 = kani::any();
        let mut payload = [0u8; 102];
        let bytes = amount.to_be_bytes();
        payload[86..102].copy_from_slice(&bytes);
        let recovered = decode_payload_src_amount(&payload);
        kani::assert(recovered == Some(amount), "decoded amount must match original");
    }

    // ── HARNESS 11 ─────────────────────────────────────────────────────────────
    /// **Property:** `slash_bps_for_tier` is monotonically non-increasing.
    ///
    /// Higher-tier solvers face lower (or equal) slash rates; this is a
    /// correctness property of the tier-perk table.
    #[kani::proof]
    #[kani::unwind(6)]
    fn kani_slash_bps_monotone() {
        for tier in 0u32..4 {
            kani::assert(
                slash_bps_for_tier(tier) >= slash_bps_for_tier(tier + 1),
                "slash rate must not increase with tier",
            );
        }
    }

    // ── HARNESS 12 ─────────────────────────────────────────────────────────────
    /// **Property:** `decode_i128_be` never panics and returns `None` for
    /// out-of-bounds offsets.
    #[kani::proof]
    fn kani_decode_i128_be_bounds_safe() {
        let bytes: [u8; 32] = kani::any();
        let offset: usize = kani::any();
        // Unconstrained call must not panic (no unwrap inside)
        let _result = decode_i128_be(&bytes, offset);
        // When offset + 16 > 32 the result must be None
        if offset > 16 {
            kani::assert(
                decode_i128_be(&bytes, offset).is_none(),
                "out-of-bounds decode must return None",
            );
        }
    }
}
