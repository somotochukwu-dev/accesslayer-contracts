use crate::{
    Airdrop, AirdropAllocation, AirdropClaimedEvent, AirdropCreatedEvent, HolderRewardsContract,
    HolderRewardsContractClient, HolderRewardsError, AIRDROP_CLAIMED_EVENT_NAME,
    AIRDROP_CREATED_EVENT_NAME,
};
use creator_keys::{CreatorKeysContract, CreatorKeysContractClient, RegisterCreatorParams};
use soroban_sdk::{
    testutils::{Address as _, Events},
    token::{StellarAssetClient, TokenClient},
    Address, Env, IntoVal, String, Symbol, Val, Vec,
};

const KEY_PRICE: i128 = 100;
const CREATOR_BPS: u32 = 9000;
const PROTOCOL_BPS: u32 = 1000;
const INITIAL_TOKENS: i128 = 1_000_000;

struct Setup<'a> {
    rewards: HolderRewardsContractClient<'a>,
    keys: CreatorKeysContractClient<'a>,
    rewards_id: Address,
    key_id: Address,
    admin: Address,
    token: Address,
}

fn setup(env: &Env) -> Setup<'_> {
    env.mock_all_auths();

    let keys_id = env.register(CreatorKeysContract, ());
    let keys = CreatorKeysContractClient::new(env, &keys_id);
    let admin = Address::generate(env);
    keys.set_protocol_admin(&admin, &admin);
    keys.set_key_price(&admin, &KEY_PRICE);
    keys.set_fee_config(&admin, &CREATOR_BPS, &PROTOCOL_BPS);

    let key_id = Address::generate(env);
    keys.register_creator(
        &RegisterCreatorParams {
            creator: key_id.clone(),
            handle: String::from_str(env, "alice"),
        },
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
    );

    let rewards_id = env.register(HolderRewardsContract, ());
    let rewards = HolderRewardsContractClient::new(env, &rewards_id);
    rewards.initialize(&keys_id);

    let sac = env.register_stellar_asset_contract_v2(admin.clone());

    Setup {
        rewards,
        keys,
        rewards_id,
        key_id,
        admin,
        token: sac.address(),
    }
}

fn mint(env: &Env, setup: &Setup, to: &Address, amount: i128) {
    StellarAssetClient::new(env, &setup.token).mint(to, &amount);
}

fn token_balance(env: &Env, setup: &Setup, wallet: &Address) -> i128 {
    TokenClient::new(env, &setup.token).balance(wallet)
}

fn buy_keys(setup: &Setup, buyer: &Address, count: u32) {
    for _ in 0..count {
        let quote = setup.keys.get_buy_quote(&setup.key_id);
        setup
            .keys
            .buy_key(&setup.key_id, buyer, &quote.total_amount, &None);
    }
}

fn topic_matches(env: &Env, topics: &Vec<Val>, expected: Symbol) -> bool {
    topics
        .get(0)
        .map(|value| {
            let symbol: Symbol = value.into_val(env);
            symbol == expected
        })
        .unwrap_or(false)
}

fn created_events(env: &Env, rewards_id: &Address) -> Vec<AirdropCreatedEvent> {
    let mut matching: Vec<AirdropCreatedEvent> = Vec::new(env);
    for (contract, topics, data) in env.events().all().iter() {
        if contract == *rewards_id && topic_matches(env, &topics, AIRDROP_CREATED_EVENT_NAME) {
            let event: AirdropCreatedEvent = data.into_val(env);
            matching.push_back(event);
        }
    }
    matching
}

fn claimed_events(env: &Env, rewards_id: &Address) -> Vec<AirdropClaimedEvent> {
    let mut matching: Vec<AirdropClaimedEvent> = Vec::new(env);
    for (contract, topics, data) in env.events().all().iter() {
        if contract == *rewards_id && topic_matches(env, &topics, AIRDROP_CLAIMED_EVENT_NAME) {
            let event: AirdropClaimedEvent = data.into_val(env);
            matching.push_back(event);
        }
    }
    matching
}

/// Registers `creator` with three holders: alice (3 keys), bob (1 key).
fn setup_holders(env: &Env) -> (Setup<'_>, Address, Address) {
    let setup = setup(env);
    let alice = Address::generate(env);
    let bob = Address::generate(env);
    buy_keys(&setup, &alice, 3);
    buy_keys(&setup, &bob, 1);
    mint(env, &setup, &setup.key_id, INITIAL_TOKENS);
    (setup, alice, bob)
}

#[test]
fn test_create_airdrop_snapshots_holders_and_emits_event() {
    let env = Env::default();
    let (setup, alice, bob) = setup_holders(&env);
    assert_eq!(setup.keys.get_key_balance(&setup.key_id, &alice), 3);
    assert_eq!(setup.keys.get_key_balance(&setup.key_id, &bob), 1);

    let snapshot_ledger = env.ledger().sequence();
    let airdrop_id =
        setup
            .rewards
            .create_airdrop(&setup.key_id, &setup.token, &1_000i128, &setup.key_id);
    assert_eq!(airdrop_id, 1);

    // Event assertions run before any other invocation: the test host only
    // retains the most recent invocation's events.
    let events = created_events(&env, &setup.rewards_id);
    assert_eq!(events.len(), 1);
    let event = events.get(0).unwrap();
    assert_eq!(event.airdrop_id, airdrop_id);
    assert_eq!(event.key_id, setup.key_id);
    assert_eq!(event.reward_token, setup.token);
    assert_eq!(event.funder, setup.key_id);
    assert_eq!(event.total_amount, 1_000);
    assert_eq!(event.snapshot_ledger, snapshot_ledger);
    assert_eq!(event.snapshot_total, 4);
    assert_eq!(event.holder_count, 2);

    let airdrop: Airdrop = setup
        .rewards
        .get_airdrop(&airdrop_id)
        .expect("created airdrop must be readable");
    assert_eq!(airdrop.airdrop_id, 1);
    assert_eq!(airdrop.key_id, setup.key_id);
    assert_eq!(airdrop.reward_token, setup.token);
    assert_eq!(airdrop.funder, setup.key_id);
    assert_eq!(airdrop.total_amount, 1_000);
    assert_eq!(airdrop.snapshot_ledger, snapshot_ledger);
    assert_eq!(airdrop.snapshot_total, 4);
    assert_eq!(airdrop.holder_count, 2);
    assert_eq!(airdrop.claimed_total, 0);

    // State assertion: the funded amount is escrowed by the rewards contract.
    assert_eq!(token_balance(&env, &setup, &setup.rewards_id), 1_000);
    assert_eq!(
        token_balance(&env, &setup, &setup.key_id),
        INITIAL_TOKENS - 1_000
    );
}

#[test]
fn test_create_airdrop_allows_protocol_admin_to_fund() {
    let env = Env::default();
    let (setup, _, _) = setup_holders(&env);
    mint(&env, &setup, &setup.admin, INITIAL_TOKENS);

    let airdrop_id =
        setup
            .rewards
            .create_airdrop(&setup.key_id, &setup.token, &5_000i128, &setup.admin);

    let airdrop = setup.rewards.get_airdrop(&airdrop_id).unwrap();
    assert_eq!(airdrop.funder, setup.admin);
    assert_eq!(token_balance(&env, &setup, &setup.rewards_id), 5_000);
    assert_eq!(
        token_balance(&env, &setup, &setup.admin),
        INITIAL_TOKENS - 5_000
    );
}

#[test]
fn test_create_airdrop_rejects_funder_who_is_neither_creator_nor_admin() {
    let env = Env::default();
    let (setup, _, _) = setup_holders(&env);
    let stranger = Address::generate(&env);
    mint(&env, &setup, &stranger, 1_000);

    let result =
        setup
            .rewards
            .try_create_airdrop(&setup.key_id, &setup.token, &1_000i128, &stranger);
    assert_eq!(result, Err(Ok(HolderRewardsError::Unauthorized)));

    // State assertion: a rejected call writes nothing and moves no tokens.
    assert_eq!(setup.rewards.get_airdrop(&1), None);
    assert_eq!(token_balance(&env, &setup, &setup.rewards_id), 0);
    assert_eq!(token_balance(&env, &setup, &stranger), 1_000);
    assert_eq!(created_events(&env, &setup.rewards_id).len(), 0);
}

#[test]
fn test_create_airdrop_rejects_non_positive_amount() {
    let env = Env::default();
    let (setup, _, _) = setup_holders(&env);

    let zero = setup
        .rewards
        .try_create_airdrop(&setup.key_id, &setup.token, &0i128, &setup.key_id);
    assert_eq!(zero, Err(Ok(HolderRewardsError::NotPositiveAmount)));

    let negative =
        setup
            .rewards
            .try_create_airdrop(&setup.key_id, &setup.token, &-100i128, &setup.key_id);
    assert_eq!(negative, Err(Ok(HolderRewardsError::NotPositiveAmount)));

    assert_eq!(setup.rewards.get_airdrop(&1), None);
}

#[test]
fn test_create_airdrop_reverts_when_key_has_no_holders() {
    let env = Env::default();
    let setup = setup(&env);
    mint(&env, &setup, &setup.key_id, INITIAL_TOKENS);

    let result =
        setup
            .rewards
            .try_create_airdrop(&setup.key_id, &setup.token, &1_000i128, &setup.key_id);
    assert_eq!(result, Err(Ok(HolderRewardsError::NoKeyHolders)));
    assert_eq!(token_balance(&env, &setup, &setup.rewards_id), 0);
}

#[test]
fn test_create_airdrop_reverts_before_initialize() {
    let env = Env::default();
    env.mock_all_auths();
    let rewards_id = env.register(HolderRewardsContract, ());
    let rewards = HolderRewardsContractClient::new(&env, &rewards_id);
    assert_eq!(rewards.get_creator_keys_contract(), None);

    let key_id = Address::generate(&env);
    let token = Address::generate(&env);
    let result = rewards.try_create_airdrop(&key_id, &token, &1_000i128, &key_id);
    assert_eq!(result, Err(Ok(HolderRewardsError::NotInitialized)));
}

#[test]
fn test_initialize_rejects_second_call() {
    let env = Env::default();
    let setup = setup(&env);
    let keys_id = setup.rewards.get_creator_keys_contract().unwrap();

    let result = setup.rewards.try_initialize(&keys_id);
    assert_eq!(result, Err(Ok(HolderRewardsError::AlreadyInitialized)));
    assert_eq!(setup.rewards.get_creator_keys_contract(), Some(keys_id));
}

#[test]
fn test_claim_airdrop_pays_pro_rata_share_and_emits_event() {
    let env = Env::default();
    let (setup, alice, bob) = setup_holders(&env);
    let airdrop_id =
        setup
            .rewards
            .create_airdrop(&setup.key_id, &setup.token, &1_000i128, &setup.key_id);

    // alice holds 3 of 4 snapshotted keys: 3 / 4 * 1000 = 750.
    let paid = setup.rewards.claim_airdrop(&airdrop_id, &alice);
    assert_eq!(paid, 750);

    let events = claimed_events(&env, &setup.rewards_id);
    assert_eq!(events.len(), 1);
    let event = events.get(0).unwrap();
    assert_eq!(event.airdrop_id, airdrop_id);
    assert_eq!(event.key_id, setup.key_id);
    assert_eq!(event.claimer, alice);
    assert_eq!(event.amount, 750);

    assert_eq!(token_balance(&env, &setup, &alice), 750);
    assert_eq!(token_balance(&env, &setup, &setup.rewards_id), 250);

    let allocation: AirdropAllocation = setup.rewards.get_airdrop_allocation(&airdrop_id, &alice);
    assert_eq!(
        allocation,
        AirdropAllocation {
            airdrop_id,
            wallet: alice.clone(),
            allocation: 750,
            claimed: 750,
            claimable: 0,
        }
    );

    // bob holds the remaining 1 of 4 keys: 1 / 4 * 1000 = 250.
    assert_eq!(setup.rewards.claim_airdrop(&airdrop_id, &bob), 250);
    let events = claimed_events(&env, &setup.rewards_id);
    assert_eq!(events.len(), 1);
    let event = events.get(0).unwrap();
    assert_eq!(event.claimer, bob);
    assert_eq!(event.amount, 250);
    assert_eq!(token_balance(&env, &setup, &bob), 250);
    assert_eq!(token_balance(&env, &setup, &setup.rewards_id), 0);

    let airdrop = setup.rewards.get_airdrop(&airdrop_id).unwrap();
    assert_eq!(airdrop.claimed_total, 1_000);
}

#[test]
fn test_claim_airdrop_reverts_on_double_claim() {
    let env = Env::default();
    let (setup, alice, _) = setup_holders(&env);
    let airdrop_id =
        setup
            .rewards
            .create_airdrop(&setup.key_id, &setup.token, &1_000i128, &setup.key_id);

    assert_eq!(setup.rewards.claim_airdrop(&airdrop_id, &alice), 750);
    assert_eq!(claimed_events(&env, &setup.rewards_id).len(), 1);

    let result = setup.rewards.try_claim_airdrop(&airdrop_id, &alice);
    assert_eq!(result, Err(Ok(HolderRewardsError::AlreadyClaimed)));

    // State assertion: the failed second claim changes nothing.
    assert_eq!(token_balance(&env, &setup, &alice), 750);
    assert_eq!(token_balance(&env, &setup, &setup.rewards_id), 250);
    let allocation = setup.rewards.get_airdrop_allocation(&airdrop_id, &alice);
    assert_eq!(allocation.claimed, 750);
    assert_eq!(allocation.claimable, 0);
    assert_eq!(
        setup
            .rewards
            .get_airdrop(&airdrop_id)
            .unwrap()
            .claimed_total,
        750
    );
}

#[test]
fn test_claim_airdrop_allocation_unaffected_by_post_snapshot_changes() {
    let env = Env::default();
    let (setup, alice, bob) = setup_holders(&env);
    let airdrop_id =
        setup
            .rewards
            .create_airdrop(&setup.key_id, &setup.token, &1_000i128, &setup.key_id);

    // Post-snapshot changes: alice moves all 3 keys to bob, carol buys 2 keys.
    let carol = Address::generate(&env);
    setup.keys.transfer_keys(&setup.key_id, &alice, &bob, &3u32);
    buy_keys(&setup, &carol, 2);
    assert_eq!(setup.keys.get_key_balance(&setup.key_id, &alice), 0);
    assert_eq!(setup.keys.get_key_balance(&setup.key_id, &bob), 4);

    // Balances move, allocations do not.
    assert_eq!(setup.rewards.claim_airdrop(&airdrop_id, &alice), 750);
    assert_eq!(setup.rewards.claim_airdrop(&airdrop_id, &bob), 250);
    assert_eq!(
        setup.rewards.try_claim_airdrop(&airdrop_id, &carol),
        Err(Ok(HolderRewardsError::NoAllocation))
    );

    let carol_allocation = setup.rewards.get_airdrop_allocation(&airdrop_id, &carol);
    assert_eq!(carol_allocation.allocation, 0);
    assert_eq!(carol_allocation.claimed, 0);
    assert_eq!(carol_allocation.claimable, 0);
    assert_eq!(token_balance(&env, &setup, &setup.rewards_id), 0);
}

#[test]
fn test_claim_airdrop_floors_allocation_to_whole_units() {
    let env = Env::default();
    let setup = setup(&env);
    let mut holders: Vec<Address> = Vec::new(&env);
    for _ in 0..3 {
        let holder = Address::generate(&env);
        buy_keys(&setup, &holder, 1);
        holders.push_back(holder.clone());
    }
    mint(&env, &setup, &setup.key_id, INITIAL_TOKENS);

    // 100 / 3 floors to 33 per holder; 1 unit of dust stays escrowed.
    let airdrop_id =
        setup
            .rewards
            .create_airdrop(&setup.key_id, &setup.token, &100i128, &setup.key_id);
    for holder in holders.iter() {
        assert_eq!(setup.rewards.claim_airdrop(&airdrop_id, &holder), 33);
    }
    assert_eq!(token_balance(&env, &setup, &setup.rewards_id), 1);
    assert_eq!(
        setup
            .rewards
            .get_airdrop(&airdrop_id)
            .unwrap()
            .claimed_total,
        99
    );
}

#[test]
fn test_get_airdrop_allocation_returns_zero_for_wallet_outside_snapshot() {
    let env = Env::default();
    let (setup, alice, _) = setup_holders(&env);
    let airdrop_id =
        setup
            .rewards
            .create_airdrop(&setup.key_id, &setup.token, &1_000i128, &setup.key_id);
    let stranger = Address::generate(&env);

    let allocation = setup.rewards.get_airdrop_allocation(&airdrop_id, &stranger);
    assert_eq!(
        allocation,
        AirdropAllocation {
            airdrop_id,
            wallet: stranger,
            allocation: 0,
            claimed: 0,
            claimable: 0,
        }
    );

    // An unclaimed snapshotted holder reads claimable == allocation.
    let unclaimed = setup.rewards.get_airdrop_allocation(&airdrop_id, &alice);
    assert_eq!(unclaimed.allocation, 750);
    assert_eq!(unclaimed.claimed, 0);
    assert_eq!(unclaimed.claimable, 750);
}

#[test]
fn test_get_airdrop_allocation_reverts_for_unknown_airdrop() {
    let env = Env::default();
    let (setup, alice, _) = setup_holders(&env);

    let result = setup.rewards.try_get_airdrop_allocation(&99u64, &alice);
    assert_eq!(result, Err(Ok(HolderRewardsError::AirdropNotFound)));
    assert_eq!(
        setup.rewards.try_claim_airdrop(&99u64, &alice),
        Err(Ok(HolderRewardsError::AirdropNotFound))
    );
}

#[test]
fn test_get_airdrop_allocation_does_not_mutate_state() {
    let env = Env::default();
    let (setup, alice, _) = setup_holders(&env);
    let airdrop_id =
        setup
            .rewards
            .create_airdrop(&setup.key_id, &setup.token, &1_000i128, &setup.key_id);

    let airdrop_before: Option<Airdrop> = setup.rewards.get_airdrop(&airdrop_id);
    let first = setup.rewards.get_airdrop_allocation(&airdrop_id, &alice);
    let second = setup.rewards.get_airdrop_allocation(&airdrop_id, &alice);

    assert_eq!(first, second);
    assert_eq!(setup.rewards.get_airdrop(&airdrop_id), airdrop_before);
    assert_eq!(
        setup
            .rewards
            .get_airdrop(&airdrop_id)
            .unwrap()
            .claimed_total,
        0
    );
}
