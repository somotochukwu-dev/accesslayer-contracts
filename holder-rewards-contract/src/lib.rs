#![no_std]

//! Holder rewards airdrops distributed pro-rata to creator key holders.
//!
//! [`HolderRewardsContract::create_airdrop`] escrows a reward token and
//! snapshots every current key holder's balance at the creation block.
//! [`HolderRewardsContract::claim_airdrop`] pays each snapshotted holder their
//! pro-rata share of the escrowed amount, so keys bought, sold, or transferred
//! after the snapshot never change an allocation.
//!
//! `key_id` addresses the creator registered in the configured `creator-keys`
//! contract (see [`HolderRewardsContract::initialize`]), matching the first
//! argument of the `creator-keys` reads such as `get_holder_registry`.

#[cfg(test)]
extern crate std;

use creator_keys::CreatorKeysContractClient;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token::TokenClient, Address,
    Env, Symbol, Vec,
};

#[cfg(test)]
mod test;

pub const AIRDROP_CREATED_EVENT_NAME: Symbol = symbol_short!("aird_cr");
pub const AIRDROP_CLAIMED_EVENT_NAME: Symbol = symbol_short!("aird_clm");

pub const AIRDROP_CREATED_DATA_FIELDS: [&str; 8] = [
    "airdrop_id",
    "key_id",
    "reward_token",
    "funder",
    "total_amount",
    "snapshot_ledger",
    "snapshot_total",
    "holder_count",
];

pub const AIRDROP_CLAIMED_DATA_FIELDS: [&str; 5] =
    ["airdrop_id", "key_id", "claimer", "amount", "ledger"];

/// Default persistent storage TTL thresholds (~30 days).
pub const AIRDROP_TTL_THRESHOLD: u32 = 100_000;
pub const AIRDROP_TTL_EXTEND_TO: u32 = 518_400;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum HolderRewardsError {
    /// The escrow amount is zero or negative.
    NotPositiveAmount = 1,
    /// The funder is neither the creator nor the protocol admin.
    Unauthorized = 2,
    /// `initialize` has already been called.
    AlreadyInitialized = 3,
    /// `initialize` has not been called yet.
    NotInitialized = 4,
    /// The airdrop id does not exist.
    AirdropNotFound = 5,
    /// The wallet has already claimed this airdrop.
    AlreadyClaimed = 6,
    /// The wallet's snapshotted share rounds to nothing to claim.
    NoAllocation = 7,
    /// The key had no holders with a balance when the airdrop was created.
    NoKeyHolders = 8,
    /// Integer arithmetic would overflow.
    ArithmeticOverflow = 9,
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    /// Address of the `creator-keys` contract holder balances are read from.
    CreatorKeysContract,
    /// Next airdrop id to assign (`u64`).
    NextAirdropId,
    /// Airdrop record by airdrop id.
    Airdrop(u64),
    /// Key balance snapshotted at creation for a holder of an airdrop.
    AirdropHolder(u64, Address),
    /// Amount already claimed by a wallet for an airdrop.
    Claimed(u64, Address),
}

/// One funded airdrop and the holder snapshot it was created with.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Airdrop {
    pub airdrop_id: u64,
    /// Creator key the airdrop targets (first argument of `creator-keys` reads).
    pub key_id: Address,
    /// Escrowed reward token.
    pub reward_token: Address,
    /// Creator or protocol admin that funded the escrow.
    pub funder: Address,
    /// Total reward tokens escrowed for this airdrop.
    pub total_amount: i128,
    /// Ledger at which holder balances were snapshotted (creation ledger).
    pub snapshot_ledger: u32,
    /// Sum of all snapshotted holder balances; the pro-rata denominator.
    pub snapshot_total: u32,
    /// Number of holders with a positive balance in the snapshot.
    pub holder_count: u32,
    /// Running total of rewards claimed so far.
    pub claimed_total: i128,
}

/// Pro-rata entitlement and claim progress for one wallet on one airdrop.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct AirdropAllocation {
    pub airdrop_id: u64,
    pub wallet: Address,
    /// Full entitlement derived from the snapshot balance.
    pub allocation: i128,
    /// Amount already claimed by the wallet.
    pub claimed: i128,
    /// Remaining claimable amount (`allocation - claimed`).
    pub claimable: i128,
}

/// Event payload for `AirdropCreated`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct AirdropCreatedEvent {
    pub airdrop_id: u64,
    pub key_id: Address,
    pub reward_token: Address,
    pub funder: Address,
    pub total_amount: i128,
    pub snapshot_ledger: u32,
    pub snapshot_total: u32,
    pub holder_count: u32,
}

/// Event payload for `AirdropClaimed`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct AirdropClaimedEvent {
    pub airdrop_id: u64,
    pub key_id: Address,
    pub claimer: Address,
    pub amount: i128,
    pub ledger: u32,
}

#[contract]
pub struct HolderRewardsContract;

fn extend_ttl(env: &Env, key: &DataKey) {
    env.storage()
        .persistent()
        .extend_ttl(key, AIRDROP_TTL_THRESHOLD, AIRDROP_TTL_EXTEND_TO);
}

fn read_keys_contract(env: &Env) -> Result<Address, HolderRewardsError> {
    env.storage()
        .persistent()
        .get(&DataKey::CreatorKeysContract)
        .ok_or(HolderRewardsError::NotInitialized)
}

fn read_airdrop(env: &Env, airdrop_id: u64) -> Result<Airdrop, HolderRewardsError> {
    env.storage()
        .persistent()
        .get(&DataKey::Airdrop(airdrop_id))
        .ok_or(HolderRewardsError::AirdropNotFound)
}

/// Pro-rata floor share of the escrowed amount for a snapshotted balance.
fn allocation_for(airdrop: &Airdrop, snapshot_balance: u32) -> Result<i128, HolderRewardsError> {
    if snapshot_balance == 0 || airdrop.snapshot_total == 0 {
        return Ok(0);
    }
    airdrop
        .total_amount
        .checked_mul(i128::from(snapshot_balance))
        .ok_or(HolderRewardsError::ArithmeticOverflow)?
        .checked_div(i128::from(airdrop.snapshot_total))
        .ok_or(HolderRewardsError::ArithmeticOverflow)
}

#[contractimpl]
impl HolderRewardsContract {
    /// One-time setup: the `creator-keys` contract whose holder registry and
    /// balances back every airdrop snapshot. Call immediately after deployment;
    /// like `key-staking-contract::initialize` the call itself is
    /// permissionless and rejects a second configuration.
    pub fn initialize(env: Env, creator_keys_contract: Address) -> Result<(), HolderRewardsError> {
        if env
            .storage()
            .persistent()
            .has(&DataKey::CreatorKeysContract)
        {
            return Err(HolderRewardsError::AlreadyInitialized);
        }
        env.storage()
            .persistent()
            .set(&DataKey::CreatorKeysContract, &creator_keys_contract);
        extend_ttl(&env, &DataKey::CreatorKeysContract);
        Ok(())
    }

    /// Funds a new airdrop for `key_id`'s current holders and snapshots their
    /// balances at this ledger.
    ///
    /// `funder` must be the creator (`key_id`) or the `creator-keys` protocol
    /// admin and must authorize the call; `total_amount` of `reward_token` is
    /// escrowed in this contract. Holders with a positive balance are captured
    /// from the holder registry before any state is written, so a rejected
    /// call leaves no partial snapshot behind. Returns the new `airdrop_id`.
    pub fn create_airdrop(
        env: Env,
        key_id: Address,
        reward_token: Address,
        total_amount: i128,
        funder: Address,
    ) -> Result<u64, HolderRewardsError> {
        funder.require_auth();

        if total_amount <= 0 {
            return Err(HolderRewardsError::NotPositiveAmount);
        }

        let keys_contract = read_keys_contract(&env)?;
        let keys = CreatorKeysContractClient::new(&env, &keys_contract);
        if funder != key_id && keys.get_protocol_admin().as_ref() != Some(&funder) {
            return Err(HolderRewardsError::Unauthorized);
        }

        // Snapshot pass: read every positive balance before writing anything so
        // a rejected snapshot leaves no partial state behind.
        let registry = keys.get_holder_registry(&key_id);
        let mut holders: Vec<(Address, u32)> = Vec::new(&env);
        let mut snapshot_total: u32 = 0;
        for holder in registry.iter() {
            let balance = keys.get_key_balance(&key_id, &holder);
            if balance == 0 {
                continue;
            }
            snapshot_total = snapshot_total
                .checked_add(balance)
                .ok_or(HolderRewardsError::ArithmeticOverflow)?;
            holders.push_back((holder, balance));
        }
        if holders.is_empty() {
            return Err(HolderRewardsError::NoKeyHolders);
        }

        // Escrow the funded reward tokens.
        TokenClient::new(&env, &reward_token).transfer(
            &funder,
            &env.current_contract_address(),
            &total_amount,
        );

        let airdrop_id: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::NextAirdropId)
            .unwrap_or(1);
        let next_id = airdrop_id
            .checked_add(1)
            .ok_or(HolderRewardsError::ArithmeticOverflow)?;
        env.storage()
            .persistent()
            .set(&DataKey::NextAirdropId, &next_id);
        extend_ttl(&env, &DataKey::NextAirdropId);

        let snapshot_ledger = env.ledger().sequence();
        let airdrop = Airdrop {
            airdrop_id,
            key_id: key_id.clone(),
            reward_token: reward_token.clone(),
            funder: funder.clone(),
            total_amount,
            snapshot_ledger,
            snapshot_total,
            holder_count: holders.len(),
            claimed_total: 0,
        };
        let airdrop_key = DataKey::Airdrop(airdrop_id);
        env.storage().persistent().set(&airdrop_key, &airdrop);
        extend_ttl(&env, &airdrop_key);

        for (holder, balance) in holders.iter() {
            let holder_key = DataKey::AirdropHolder(airdrop_id, holder.clone());
            env.storage().persistent().set(&holder_key, &balance);
            extend_ttl(&env, &holder_key);
        }

        env.events().publish(
            (AIRDROP_CREATED_EVENT_NAME, key_id.clone(), funder.clone()),
            AirdropCreatedEvent {
                airdrop_id,
                key_id,
                reward_token,
                funder,
                total_amount,
                snapshot_ledger,
                snapshot_total,
                holder_count: airdrop.holder_count,
            },
        );

        Ok(airdrop_id)
    }

    /// Pays `claimer` their pro-rata share of `airdrop_id` from the escrow and
    /// records the claim. Only the snapshotted balance is used, so keys moved
    /// after the snapshot do not change the payout. A second call reverts with
    /// [`HolderRewardsError::AlreadyClaimed`]. Returns the amount paid.
    pub fn claim_airdrop(
        env: Env,
        airdrop_id: u64,
        claimer: Address,
    ) -> Result<i128, HolderRewardsError> {
        claimer.require_auth();

        let mut airdrop = read_airdrop(&env, airdrop_id)?;

        let claimed_key = DataKey::Claimed(airdrop_id, claimer.clone());
        if env.storage().persistent().has(&claimed_key) {
            return Err(HolderRewardsError::AlreadyClaimed);
        }

        let snapshot_balance: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::AirdropHolder(airdrop_id, claimer.clone()))
            .unwrap_or(0);
        let amount = allocation_for(&airdrop, snapshot_balance)?;
        if amount <= 0 {
            return Err(HolderRewardsError::NoAllocation);
        }

        env.storage().persistent().set(&claimed_key, &amount);
        extend_ttl(&env, &claimed_key);

        airdrop.claimed_total = airdrop
            .claimed_total
            .checked_add(amount)
            .ok_or(HolderRewardsError::ArithmeticOverflow)?;
        let airdrop_key = DataKey::Airdrop(airdrop_id);
        env.storage().persistent().set(&airdrop_key, &airdrop);
        extend_ttl(&env, &airdrop_key);

        TokenClient::new(&env, &airdrop.reward_token).transfer(
            &env.current_contract_address(),
            &claimer,
            &amount,
        );

        env.events().publish(
            (
                AIRDROP_CLAIMED_EVENT_NAME,
                airdrop.key_id.clone(),
                claimer.clone(),
            ),
            AirdropClaimedEvent {
                airdrop_id,
                key_id: airdrop.key_id,
                claimer,
                amount,
                ledger: env.ledger().sequence(),
            },
        );

        Ok(amount)
    }

    /// Pro-rata entitlement, amount claimed, and remaining claimable for
    /// `wallet` on `airdrop_id`. A wallet outside the snapshot reads as zero;
    /// only an unknown airdrop id reverts. Read-only: never mutates storage.
    pub fn get_airdrop_allocation(
        env: Env,
        airdrop_id: u64,
        wallet: Address,
    ) -> Result<AirdropAllocation, HolderRewardsError> {
        let airdrop = read_airdrop(&env, airdrop_id)?;
        let snapshot_balance: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::AirdropHolder(airdrop_id, wallet.clone()))
            .unwrap_or(0);
        let allocation = allocation_for(&airdrop, snapshot_balance)?;
        let claimed: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Claimed(airdrop_id, wallet.clone()))
            .unwrap_or(0);
        let claimable = allocation.saturating_sub(claimed);
        Ok(AirdropAllocation {
            airdrop_id,
            wallet,
            allocation,
            claimed,
            claimable,
        })
    }

    /// Full airdrop record, including its snapshot metadata, or `None` for an
    /// unknown id.
    pub fn get_airdrop(env: Env, airdrop_id: u64) -> Option<Airdrop> {
        env.storage()
            .persistent()
            .get(&DataKey::Airdrop(airdrop_id))
    }

    /// The configured `creator-keys` contract, before and after initialization.
    pub fn get_creator_keys_contract(env: Env) -> Option<Address> {
        env.storage()
            .persistent()
            .get(&DataKey::CreatorKeysContract)
    }
}
