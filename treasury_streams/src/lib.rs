#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env};

/// Storage keys for the treasury streams contract.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Address allowed to create/cancel streams (timelock or governor).
    Governor,
    /// Monotonic counter for stream ids.
    NextId,
    /// Per-stream record.
    Stream(u64),
}

/// A single payment stream.
#[contracttype]
#[derive(Clone)]
pub struct Stream {
    pub sender: Address,
    pub recipient: Address,
    pub token: Address,
    pub total: i128,
    pub start: u64,
    pub cliff: u64,
    pub end: u64,
    pub withdrawn: i128,
    pub cancelled: bool,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    Unauthorized = 3,
    StreamNotFound = 4,
    InvalidSchedule = 5,
    InvalidAmount = 6,
    NothingToWithdraw = 7,
    AlreadyCancelled = 8,
}

#[contract]
pub struct TreasuryStreams;

#[contractimpl]
impl TreasuryStreams {
    /// One-time initialization setting the governor (timelock/governor) address.
    pub fn initialize(env: Env, governor: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Governor) {
            return Err(Error::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Governor, &governor);
        env.storage().instance().set(&DataKey::NextId, &0u64);
        Ok(())
    }

    /// Creates a new stream funded by `sender`. Only the governor may call this.
    pub fn create_stream(
        env: Env,
        sender: Address,
        recipient: Address,
        token: Address,
        total: i128,
        start: u64,
        cliff: u64,
        end: u64,
    ) -> Result<u64, Error> {
        let governor: Address = env
            .storage()
            .instance()
            .get(&DataKey::Governor)
            .ok_or(Error::NotInitialized)?;
        governor.require_auth();

        if total <= 0 {
            return Err(Error::InvalidAmount);
        }
        if end <= start || cliff < start || cliff > end {
            return Err(Error::InvalidSchedule);
        }

        // Escrow the full deposit into the contract up front.
        token::Client::new(&env, &token).transfer(&sender, &env.current_contract_address(), &total);

        let id: u64 = env.storage().instance().get(&DataKey::NextId).unwrap_or(0);
        let stream = Stream {
            sender: sender.clone(),
            recipient: recipient.clone(),
            token: token.clone(),
            total,
            start,
            cliff,
            end,
            withdrawn: 0,
            cancelled: false,
        };
        env.storage().persistent().set(&DataKey::Stream(id), &stream);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));

        env.events().publish(
            (soroban_sdk::symbol_short!("create"), id),
            (sender, recipient, token, total, start, cliff, end),
        );
        Ok(id)
    }

    /// Pull-based withdrawal of all currently vested funds for a stream.
    pub fn withdraw(env: Env, stream_id: u64) -> Result<i128, Error> {
        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .ok_or(Error::StreamNotFound)?;

        stream.recipient.require_auth();

        let vested = Self::vested_amount(&stream, env.ledger().timestamp());
        let claimable = vested - stream.withdrawn;
        if claimable <= 0 {
            return Err(Error::NothingToWithdraw);
        }

        stream.withdrawn = vested;
        env.storage().persistent().set(&DataKey::Stream(stream_id), &stream);

        token::Client::new(&env, &stream.token).transfer(
            &env.current_contract_address(),
            &stream.recipient,
            &claimable,
        );

        env.events().publish(
            (soroban_sdk::symbol_short!("withdraw"), stream_id),
            (stream.recipient.clone(), claimable),
        );
        Ok(claimable)
    }

    /// Cancels a stream. Unvested funds return to the sender; vested funds stay
    /// withdrawable by the recipient. Only the governor may call this.
    pub fn cancel(env: Env, stream_id: u64) -> Result<(), Error> {
        let governor: Address = env
            .storage()
            .instance()
            .get(&DataKey::Governor)
            .ok_or(Error::NotInitialized)?;
        governor.require_auth();

        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .ok_or(Error::StreamNotFound)?;
        if stream.cancelled {
            return Err(Error::AlreadyCancelled);
        }

        let vested = Self::vested_amount(&stream, env.ledger().timestamp());
        let unvested = stream.total - vested;

        stream.cancelled = true;
        // Cap the stream at what has vested so far; the recipient keeps the
        // right to withdraw the vested remainder.
        stream.total = vested;
        env.storage().persistent().set(&DataKey::Stream(stream_id), &stream);

        if unvested > 0 {
            token::Client::new(&env, &stream.token).transfer(
                &env.current_contract_address(),
                &stream.sender,
                &unvested,
            );
        }

        env.events().publish(
            (soroban_sdk::symbol_short!("cancel"), stream_id),
            (stream.sender.clone(), unvested),
        );
        Ok(())
    }

    /// Returns the currently withdrawable (vested but not yet withdrawn) balance.
    pub fn balance_of(env: Env, stream_id: u64) -> Result<i128, Error> {
        let stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .ok_or(Error::StreamNotFound)?;
        let vested = Self::vested_amount(&stream, env.ledger().timestamp());
        Ok(vested - stream.withdrawn)
    }

    /// Returns the full stream record.
    pub fn get_stream(env: Env, stream_id: u64) -> Result<Stream, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .ok_or(Error::StreamNotFound)
    }

    /// Precision-safe vested amount: `total * elapsed / duration` using integer
    /// math. The final withdrawal sweeps any rounding dust because once
    /// `now >= end` the full `total` is considered vested.
    fn vested_amount(stream: &Stream, now: u64) -> i128 {
        if now < stream.cliff {
            return 0;
        }
        if now >= stream.end {
            return stream.total;
        }
        let duration = (stream.end - stream.start) as i128;
        let elapsed = (now - stream.start) as i128;
        stream.total * elapsed / duration
    }
}

#[cfg(test)]
mod test;
