#![no_std]

//! Vortex Protocol — Backstop LP Vault (`backstop_vault`) — Issue #370
//!
//! A standalone Soroban contract where liquidity providers deposit the bond
//! token (USDC) in exchange for vault shares. The vault:
//!
//! - Absorbs user-compensation claims via `cover()` (callable only by the
//!   settlement contract address set at initialization).
//! - Earns a configurable share of protocol fees and slash proceeds routed
//!   by `intent_settlement` via `receive_income()`.
//! - Issues pro-rata shares that appreciate as the vault earns income and
//!   depreciate as it pays out claims (loss socialization across all LPs).
//! - Enforces a deposit-triggered cooldown to prevent withdrawal front-running
//!   ahead of a known fee inflow.
//! - Supports an emergency withdrawal-queue mode when the vault cannot service
//!   all withdrawals immediately (bank-run protection).
//!
//! ## Share accounting (inflation-attack protection)
//!
//! The vault is seeded with `VIRTUAL_SHARES` / `VIRTUAL_ASSETS` at
//! initialization so the initial exchange rate starts at 1:1 and a dust
//! first-deposit cannot manipulate the rate to an extreme.
//!
//! ```text
//! shares_minted  = deposit_amount  * total_shares / total_assets
//! assets_out     = shares_redeemed * total_assets / total_shares
//! ```

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, token,
    Address, BytesN, Env, Symbol,
};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Virtual shares/assets seeded at initialization to prevent inflation attacks.
const VIRTUAL_SHARES: i128 = 1_000_000_000; // 100 USDC-equivalent at 1:1
const VIRTUAL_ASSETS: i128 = 1_000_000_000;

/// Minimum deposit (1 USDC in 7-decimal units).
const MIN_DEPOSIT: i128 = 10_000_000;

/// Withdrawal cooldown in seconds. An LP who just deposited must wait this
/// long before they can withdraw, preventing deposit front-running ahead of
/// a known incoming fee or slash payment.
const WITHDRAWAL_COOLDOWN_SECS: u64 = 172_800; // 48 hours

const DAY_IN_LEDGERS: u32 = 17_280; // ~5 s per ledger
const PERSISTENT_TTL_THRESHOLD: u32 = DAY_IN_LEDGERS * 14;
const PERSISTENT_TTL_EXTEND_TO: u32 = DAY_IN_LEDGERS * 30;
const INSTANCE_TTL_THRESHOLD: u32 = DAY_IN_LEDGERS * 30;
const INSTANCE_TTL_EXTEND_TO: u32 = DAY_IN_LEDGERS * 60;

// ─── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Admin address (set in `initialize`).
    Admin,
    /// Bond token address (USDC or equivalent).
    BondToken,
    /// Settlement contract authorized to call `cover` and `receive_income`.
    Settlement,
    /// Total shares issued (i128).
    TotalShares,
    /// Total assets held by the vault (i128).
    TotalAssets,
    /// Per-LP share balance (i128).
    Shares(Address),
    /// Per-LP withdrawal cooldown: earliest allowed withdrawal timestamp (u64).
    WithdrawCooldown(Address),
    /// Emergency mode flag (bool). When `true`, withdrawals queue instead of
    /// executing immediately.
    EmergencyMode,
    /// Per-LP queued withdrawal amount (i128).
    WithdrawQueue(Address),
    /// Total queued withdrawal amount (i128).
    TotalQueued,
    /// Cumulative cover paid out (i128) — informational.
    TotalCoverPaid,
    /// Cumulative income received (fees + slashes) (i128).
    TotalIncome,
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    ZeroAmount = 4,
    BelowMinDeposit = 5,
    CooldownNotExpired = 6,
    InsufficientShares = 7,
    VaultInsolvent = 8,
    EmergencyModeActive = 9,
    EmergencyModeNotActive = 10,
    NothingQueued = 11,
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct BackstopVault;

#[contractimpl]
impl BackstopVault {
    // ── Initialization ────────────────────────────────────────────────────────

    /// One-time setup. Records the admin, bond_token, and authorized settlement
    /// contract. Seeds virtual shares/assets for inflation-attack resistance.
    pub fn initialize(
        env: Env,
        admin: Address,
        bond_token: Address,
        settlement: Address,
    ) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::BondToken, &bond_token);
        env.storage().instance().set(&DataKey::Settlement, &settlement);
        // Seed virtual entries.
        env.storage().instance().set(&DataKey::TotalShares, &VIRTUAL_SHARES);
        env.storage().instance().set(&DataKey::TotalAssets, &VIRTUAL_ASSETS);
        env.storage().instance().set(&DataKey::TotalCoverPaid, &0i128);
        env.storage().instance().set(&DataKey::TotalIncome, &0i128);
        env.storage().instance().set(&DataKey::TotalQueued, &0i128);
        env.storage().instance().set(&DataKey::EmergencyMode, &false);
        Self::bump_instance_ttl(&env);
    }

    // ── LP operations ─────────────────────────────────────────────────────────

    /// Deposit `amount` of bond_token and receive vault shares.
    ///
    /// Shares are minted proportional to the current assets:shares ratio.
    /// Starts a new withdrawal cooldown on the depositing LP's account to
    /// prevent deposit front-running ahead of incoming income.
    ///
    /// Returns the number of shares minted.
    pub fn deposit(env: Env, lp: Address, amount: i128) -> i128 {
        lp.require_auth();
        Self::bump_instance_ttl(&env);

        if amount < MIN_DEPOSIT {
            panic_with_error!(&env, Error::BelowMinDeposit);
        }

        let total_assets = Self::load_total_assets(&env);
        let total_shares = Self::load_total_shares(&env);

        // shares_minted = amount * total_shares / total_assets
        // Virtual entries guarantee total_assets > 0 always.
        let shares_minted = amount
            .checked_mul(total_shares)
            .unwrap_or(amount)
            .checked_div(total_assets)
            .unwrap_or(1)
            .max(1);

        // Transfer bond_token from LP to vault.
        let bond_token: Address = env.storage().instance().get(&DataKey::BondToken).unwrap();
        token::Client::new(&env, &bond_token)
            .transfer(&lp, &env.current_contract_address(), &amount);

        // Update state.
        let lp_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(lp.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::Shares(lp.clone()), &(lp_shares + shares_minted));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares + shares_minted));
        env.storage()
            .instance()
            .set(&DataKey::TotalAssets, &(total_assets + amount));
        Self::bump_persistent_ttl(&env, &DataKey::Shares(lp.clone()));

        // Reset cooldown: fresh deposit starts a new window.
        let cooldown_until = env.ledger().timestamp() + WITHDRAWAL_COOLDOWN_SECS;
        env.storage()
            .persistent()
            .set(&DataKey::WithdrawCooldown(lp.clone()), &cooldown_until);
        Self::bump_persistent_ttl(&env, &DataKey::WithdrawCooldown(lp.clone()));

        env.events().publish(
            (Symbol::new(&env, "deposit"), lp),
            (amount, shares_minted),
        );

        shares_minted
    }

    /// Redeem `shares` for bond_token assets. Subject to withdrawal cooldown.
    ///
    /// In emergency mode the withdrawal is queued and shares are burned
    /// immediately; call `process_queued_withdrawal` once funds are available.
    ///
    /// Returns the number of assets redeemed (or queued in emergency mode).
    pub fn withdraw(env: Env, lp: Address, shares: i128) -> i128 {
        lp.require_auth();
        Self::bump_instance_ttl(&env);

        if shares <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }

        let lp_shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(lp.clone()))
            .unwrap_or(0);
        if lp_shares < shares {
            panic_with_error!(&env, Error::InsufficientShares);
        }

        // Cooldown guard.
        let now = env.ledger().timestamp();
        let cooldown_until: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::WithdrawCooldown(lp.clone()))
            .unwrap_or(0);
        if now < cooldown_until {
            panic_with_error!(&env, Error::CooldownNotExpired);
        }

        let total_assets = Self::load_total_assets(&env);
        let total_shares = Self::load_total_shares(&env);

        // assets_out = shares * total_assets / total_shares
        let assets_out = shares
            .checked_mul(total_assets)
            .unwrap_or(shares)
            .checked_div(total_shares)
            .unwrap_or(0)
            .max(0);

        // Emergency mode: queue the withdrawal, burn shares immediately.
        let emergency: bool = env
            .storage()
            .instance()
            .get(&DataKey::EmergencyMode)
            .unwrap_or(false);
        if emergency {
            let queued: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::WithdrawQueue(lp.clone()))
                .unwrap_or(0);
            env.storage()
                .persistent()
                .set(&DataKey::WithdrawQueue(lp.clone()), &(queued + assets_out));
            let total_queued: i128 = env
                .storage()
                .instance()
                .get(&DataKey::TotalQueued)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::TotalQueued, &(total_queued + assets_out));
            // Burn shares immediately.
            env.storage()
                .persistent()
                .set(&DataKey::Shares(lp.clone()), &(lp_shares - shares));
            env.storage()
                .instance()
                .set(&DataKey::TotalShares, &(total_shares - shares));
            Self::bump_persistent_ttl(&env, &DataKey::Shares(lp.clone()));
            Self::bump_persistent_ttl(&env, &DataKey::WithdrawQueue(lp.clone()));
            env.events().publish(
                (Symbol::new(&env, "withdraw_queued"), lp),
                (shares, assets_out),
            );
            return assets_out;
        }

        if assets_out > total_assets {
            panic_with_error!(&env, Error::VaultInsolvent);
        }

        // Normal path: burn shares, update totals, transfer assets.
        env.storage()
            .persistent()
            .set(&DataKey::Shares(lp.clone()), &(lp_shares - shares));
        env.storage()
            .instance()
            .set(&DataKey::TotalShares, &(total_shares - shares));
        env.storage()
            .instance()
            .set(&DataKey::TotalAssets, &(total_assets - assets_out));
        Self::bump_persistent_ttl(&env, &DataKey::Shares(lp.clone()));

        let bond_token: Address = env.storage().instance().get(&DataKey::BondToken).unwrap();
        token::Client::new(&env, &bond_token)
            .transfer(&env.current_contract_address(), &lp, &assets_out);

        env.events().publish(
            (Symbol::new(&env, "withdraw"), lp),
            (shares, assets_out),
        );

        assets_out
    }

    /// Process a queued withdrawal for `lp` (emergency mode only).
    /// Pays out up to the queued amount from available assets.
    pub fn process_queued_withdrawal(env: Env, lp: Address) {
        Self::bump_instance_ttl(&env);
        let emergency: bool = env
            .storage()
            .instance()
            .get(&DataKey::EmergencyMode)
            .unwrap_or(false);
        if !emergency {
            panic_with_error!(&env, Error::EmergencyModeNotActive);
        }
        let queued: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::WithdrawQueue(lp.clone()))
            .unwrap_or(0);
        if queued <= 0 {
            panic_with_error!(&env, Error::NothingQueued);
        }
        let total_assets = Self::load_total_assets(&env);
        let payout = queued.min(total_assets);
        env.storage()
            .persistent()
            .set(&DataKey::WithdrawQueue(lp.clone()), &0i128);
        let total_queued: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalQueued)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalQueued, &(total_queued.saturating_sub(queued)));
        env.storage()
            .instance()
            .set(&DataKey::TotalAssets, &(total_assets - payout));

        let bond_token: Address = env.storage().instance().get(&DataKey::BondToken).unwrap();
        token::Client::new(&env, &bond_token)
            .transfer(&env.current_contract_address(), &lp, &payout);

        env.events().publish(
            (Symbol::new(&env, "queued_withdrawal_processed"), lp),
            payout,
        );
    }

    // ── Settlement-only callbacks ──────────────────────────────────────────────

    /// Pay `amount` of bond_token to `recipient` as backstop compensation.
    ///
    /// Only callable by the settlement contract registered at initialization.
    /// Loss is socialized across all shares by reducing `TotalAssets`.
    pub fn cover(
        env: Env,
        intent_id: BytesN<32>,
        amount: i128,
        recipient: Address,
    ) {
        Self::bump_instance_ttl(&env);
        let settlement: Address = env
            .storage()
            .instance()
            .get(&DataKey::Settlement)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NotInitialized));
        settlement.require_auth();

        if amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }

        let total_assets = Self::load_total_assets(&env);
        let payout = amount.min(total_assets);

        // Socialize loss across all shares.
        env.storage()
            .instance()
            .set(&DataKey::TotalAssets, &(total_assets - payout));
        let total_cover: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalCoverPaid)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalCoverPaid, &(total_cover + payout));

        let bond_token: Address = env.storage().instance().get(&DataKey::BondToken).unwrap();
        token::Client::new(&env, &bond_token)
            .transfer(&env.current_contract_address(), &recipient, &payout);

        env.events().publish(
            (Symbol::new(&env, "cover"), recipient),
            (intent_id, payout),
        );
    }

    /// Notify the vault that `amount` of bond_token income has been transferred
    /// to the contract address (fee share or slash proceeds).
    ///
    /// Increases `TotalAssets` so all existing share holders benefit
    /// proportionally. The caller (settlement) must have already transferred
    /// the tokens before calling this function.
    pub fn receive_income(env: Env, amount: i128) {
        Self::bump_instance_ttl(&env);
        let settlement: Address = env
            .storage()
            .instance()
            .get(&DataKey::Settlement)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NotInitialized));
        settlement.require_auth();

        if amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }
        let total_assets = Self::load_total_assets(&env);
        env.storage()
            .instance()
            .set(&DataKey::TotalAssets, &(total_assets + amount));
        let total_income: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalIncome)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalIncome, &(total_income + amount));
        env.events()
            .publish((Symbol::new(&env, "income_received"),), amount);
    }

    // ── Admin ─────────────────────────────────────────────────────────────────

    /// Admin-only: toggle emergency withdrawal-queue mode.
    /// When enabled, `withdraw` queues redemptions instead of paying
    /// immediately, protecting the vault during high-stress periods.
    pub fn set_emergency_mode(env: Env, enabled: bool) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NotInitialized));
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::EmergencyMode, &enabled);
        env.events()
            .publish((Symbol::new(&env, "emergency_mode"),), enabled);
    }

    // ── Views ──────────────────────────────────────────────────────────────────

    /// Returns (total_assets, total_shares, assets_per_share_in_bps).
    /// `assets_per_share_in_bps = total_assets * 10_000 / total_shares`
    /// (i.e. 10_000 means 1:1, 12_000 means the vault has appreciated 20%).
    pub fn get_vault_state(env: Env) -> (i128, i128, i128) {
        let total_assets = Self::load_total_assets(&env);
        let total_shares = Self::load_total_shares(&env);
        let rate = total_assets
            .checked_mul(10_000)
            .unwrap_or(total_assets)
            .checked_div(total_shares)
            .unwrap_or(10_000);
        (total_assets, total_shares, rate)
    }

    /// Returns (share_balance, queued_withdrawal_amount) for `lp`.
    pub fn get_lp_position(env: Env, lp: Address) -> (i128, i128) {
        let shares: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Shares(lp.clone()))
            .unwrap_or(0);
        let queued: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::WithdrawQueue(lp))
            .unwrap_or(0);
        (shares, queued)
    }

    // ── Private helpers ────────────────────────────────────────────────────────

    fn load_total_assets(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalAssets)
            .unwrap_or(VIRTUAL_ASSETS)
    }

    fn load_total_shares(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalShares)
            .unwrap_or(VIRTUAL_SHARES)
    }

    fn bump_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
    }

    fn bump_persistent_ttl(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND_TO);
    }
}
