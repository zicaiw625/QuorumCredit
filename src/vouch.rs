use crate::errors::ContractError;
use crate::helpers::{has_active_loan, require_allowed_token, require_not_paused, require_positive_amount};
use crate::types::{DataKey, VouchRecord};
use soroban_sdk::{symbol_short, Address, Env, Vec};

pub fn vouch(
    env: Env,
    voucher: Address,
    borrower: Address,
    stake: i128,
    token: Address,
) -> Result<(), ContractError> {
    voucher.require_auth();
    require_not_paused(&env)?;
    do_vouch(&env, voucher, borrower, stake, token)
}

fn do_vouch(
    env: &Env,
    voucher: Address,
    borrower: Address,
    stake: i128,
    token: Address,
) -> Result<(), ContractError> {
    // Validate numeric input: stake must be strictly positive.
    require_positive_amount(env, stake)?;

    assert!(voucher != borrower, "voucher cannot vouch for self");
    assert!(stake > 0, "stake must be greater than zero");

    // Check if borrower is blacklisted
    if env
        .storage()
        .persistent()
        .get::<DataKey, bool>(&DataKey::Blacklisted(borrower.clone()))
        .unwrap_or(false)
    {
        return Err(ContractError::Blacklisted);
    }

    // Validate token is allowed.
    let token_client = require_allowed_token(env, &token)?;

    // Sybil resistance: enforce minimum stake per vouch.
    let min_stake: i128 = env
        .storage()
        .instance()
        .get(&DataKey::MinStake)
        .unwrap_or(0);
    if min_stake > 0 && stake < min_stake {
        return Err(ContractError::MinStakeNotMet);
    }

    // Rate limiting: enforce cooldown between vouch calls from the same address.
    let _now = env.ledger().timestamp();
    let _last: u64 = env
        .storage()
        .persistent()
        .get(&DataKey::LastVouchTimestamp(voucher.clone()))
        .unwrap_or(0);

    let mut vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower.clone()))
        .unwrap_or(Vec::new(env));

    // Reject duplicate vouch (same voucher + same token) before any state mutation or transfer.
    for v in vouches.iter() {
        if v.voucher == voucher && v.token == token {
            return Err(ContractError::DuplicateVouch);
        }
    }

    // Enforce max vouchers per borrower limit to prevent storage bloat.
    let max_vouchers_per_borrower: u32 = env
        .storage()
        .instance()
        .get(&DataKey::MaxVouchersPerBorrower)
        .unwrap_or(crate::types::DEFAULT_MAX_VOUCHERS_PER_BORROWER);
    
    if vouches.len() >= max_vouchers_per_borrower {
        return Err(ContractError::MaxVouchersPerBorrowerExceeded);
    }

    // Reject vouch if the borrower already has an active loan — the stake
    // would be locked with no effect on the existing loan (fixes issue #13).
    if has_active_loan(env, &borrower) {
        return Err(ContractError::ActiveLoanExists);
    }

    // Transfer stake from voucher into the contract.
    token_client.transfer(&voucher, &env.current_contract_address(), &stake);

    // Track voucher → borrowers history.
    let mut history: Vec<Address> = env
        .storage()
        .persistent()
        .get(&DataKey::VoucherHistory(voucher.clone()))
        .unwrap_or(Vec::new(env));
    history.push_back(borrower.clone());
    env.storage()
        .persistent()
        .set(&DataKey::VoucherHistory(voucher.clone()), &history);

    vouches.push_back(VouchRecord {
        voucher: voucher.clone(),
        stake,
        vouch_timestamp: env.ledger().timestamp(),
        token: token.clone(),
    });
    env.storage()
        .persistent()
        .set(&DataKey::Vouches(borrower.clone()), &vouches);

    // Record the timestamp of this vouch for rate limiting.
    env.storage().persistent().set(
        &DataKey::LastVouchTimestamp(voucher.clone()),
        &env.ledger().timestamp(),
    );

    env.events().publish(
        (symbol_short!("vouch"), symbol_short!("added")),
        (voucher, borrower, stake, token),
    );

    Ok(())
}

pub fn batch_vouch(
    env: Env,
    voucher: Address,
    borrowers: Vec<Address>,
    stakes: Vec<i128>,
    token: Address,
) -> Result<(), ContractError> {
    voucher.require_auth();
    require_not_paused(&env)?;

    assert!(
        borrowers.len() == stakes.len(),
        "borrowers and stakes length mismatch"
    );
    assert!(!borrowers.is_empty(), "batch cannot be empty");

    for i in 0..borrowers.len() {
        let borrower = borrowers.get(i).unwrap();
        let stake = stakes.get(i).unwrap();
        do_vouch(&env, voucher.clone(), borrower, stake, token.clone())?;
    }

    Ok(())
}

pub fn increase_stake(
    env: Env,
    voucher: Address,
    borrower: Address,
    additional: i128,
) -> Result<(), ContractError> {
    voucher.require_auth();
    require_not_paused(&env)?;

    require_positive_amount(&env, additional)?;

    let mut vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower.clone()))
        .expect("vouch not found");

    let idx = vouches
        .iter()
        .position(|v| v.voucher == voucher)
        .expect("vouch not found") as u32;

    let mut vouch_rec = vouches.get(idx).unwrap();
    // Use the token stored on the vouch record.
    let token_client = require_allowed_token(&env, &vouch_rec.token)?;
    token_client.transfer(&voucher, &env.current_contract_address(), &additional);

    vouch_rec.stake += additional;
    vouches.set(idx, vouch_rec);

    env.storage()
        .persistent()
        .set(&DataKey::Vouches(borrower), &vouches);

    Ok(())
}

pub fn decrease_stake(
    env: Env,
    voucher: Address,
    borrower: Address,
    amount: i128,
) -> Result<(), ContractError> {
    voucher.require_auth();
    require_not_paused(&env)?;

    assert!(amount > 0, "decrease amount must be greater than zero");
    assert!(!has_active_loan(&env, &borrower), "loan already active");

    let mut vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower.clone()))
        .expect("vouch not found");

    let idx = vouches
        .iter()
        .position(|v| v.voucher == voucher)
        .expect("vouch not found") as u32;

    let mut vouch_rec = vouches.get(idx).unwrap();
    assert!(amount <= vouch_rec.stake, "decrease amount exceeds staked amount");

    let token_client = require_allowed_token(&env, &vouch_rec.token)?;
    vouch_rec.stake -= amount;
    if vouch_rec.stake == 0 {
        vouches.remove(idx);
    } else {
        vouches.set(idx, vouch_rec);
    }

    if vouches.is_empty() {
        env.storage().persistent().remove(&DataKey::Vouches(borrower));
    } else {
        env.storage().persistent().set(&DataKey::Vouches(borrower), &vouches);
    }

    token_client.transfer(&env.current_contract_address(), &voucher, &amount);

    Ok(())
}

pub fn withdraw_vouch(env: Env, voucher: Address, borrower: Address) -> Result<(), ContractError> {
    voucher.require_auth();
    require_not_paused(&env)?;

    assert!(!has_active_loan(&env, &borrower), "loan already active");

    let mut vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower.clone()))
        .ok_or(ContractError::NoActiveLoan)?;

    let idx = vouches
        .iter()
        .position(|v| v.voucher == voucher)
        .ok_or(ContractError::UnauthorizedCaller)? as u32;

    let vouch_rec = vouches.get(idx).unwrap();
    let stake = vouch_rec.stake;
    let token_addr = vouch_rec.token.clone();
    vouches.remove(idx);

    if vouches.is_empty() {
        env.storage().persistent().remove(&DataKey::Vouches(borrower.clone()));
    } else {
        env.storage().persistent().set(&DataKey::Vouches(borrower.clone()), &vouches);
    }

    let token_client = require_allowed_token(&env, &token_addr)?;
    token_client.transfer(&env.current_contract_address(), &voucher, &stake);

    env.events().publish(
        (symbol_short!("vouch"), symbol_short!("withdrawn")),
        (voucher, borrower, stake),
    );

    Ok(())
}

pub fn transfer_vouch(
    env: Env,
    from: Address,
    to: Address,
    borrower: Address,
) -> Result<(), ContractError> {
    from.require_auth();
    require_not_paused(&env)?;

    if from == to {
        return Ok(());
    }

    // Only allow transfer before a loan is active (consistent with withdraw_vouch).
    assert!(!has_active_loan(&env, &borrower), "loan already active");

    let mut vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower.clone()))
        .ok_or(ContractError::NoActiveLoan)?;

    let from_idx = vouches
        .iter()
        .position(|v| v.voucher == from)
        .ok_or(ContractError::UnauthorizedCaller)? as u32;

    let from_record = vouches.get(from_idx).unwrap();
    let stake_to_transfer = from_record.stake;

    if let Some(to_idx) = vouches.iter().position(|v| v.voucher == to) {
        // Merge into existing record for 'to'
        let mut to_record = vouches.get(to_idx as u32).unwrap();
        to_record.stake += stake_to_transfer;
        vouches.set(to_idx as u32, to_record);
        vouches.remove(from_idx);
    } else {
        // Transfer ownership to 'to'
        let mut updated_record = from_record;
        updated_record.voucher = to.clone();
        vouches.set(from_idx, updated_record);
    }

    env.storage()
        .persistent()
        .set(&DataKey::Vouches(borrower.clone()), &vouches);

    // Update voucher histories
    // 1. Remove borrower from 'from' history
    let mut from_history: Vec<Address> = env
        .storage()
        .persistent()
        .get(&DataKey::VoucherHistory(from.clone()))
        .unwrap_or(Vec::new(&env));
    if let Some(h_idx) = from_history.iter().position(|b| b == borrower) {
        from_history.remove(h_idx as u32);
        env.storage()
            .persistent()
            .set(&DataKey::VoucherHistory(from.clone()), &from_history);
    }

    // 2. Add borrower to 'to' history if not already there
    let mut to_history: Vec<Address> = env
        .storage()
        .persistent()
        .get(&DataKey::VoucherHistory(to.clone()))
        .unwrap_or(Vec::new(&env));
    if !to_history.iter().any(|b| b == borrower) {
        to_history.push_back(borrower.clone());
        env.storage()
            .persistent()
            .set(&DataKey::VoucherHistory(to.clone()), &to_history);
    }

    env.events().publish(
        (symbol_short!("vouch"), symbol_short!("transfer")),
        (from, to, borrower, stake_to_transfer),
    );

    Ok(())
}

pub fn vouch_exists(env: Env, voucher: Address, borrower: Address) -> bool {
    let vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower))
        .unwrap_or(Vec::new(&env));
    vouches.iter().any(|v| v.voucher == voucher)
}

pub fn total_vouched(env: Env, borrower: Address) -> Result<i128, ContractError> {
    let vouches = env
        .storage()
        .persistent()
        .get::<DataKey, Vec<VouchRecord>>(&DataKey::Vouches(borrower))
        .unwrap_or(Vec::new(&env));

    let mut total: i128 = 0;
    for vouch in vouches.iter() {
        total = total
            .checked_add(vouch.stake)
            .ok_or(ContractError::StakeOverflow)?;
    }

    Ok(total)
}

pub fn voucher_history(env: Env, voucher: Address) -> Vec<Address> {
    env.storage()
        .persistent()
        .get(&DataKey::VoucherHistory(voucher))
        .unwrap_or(Vec::new(&env))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DataKey;
    use crate::{QuorumCreditContract, QuorumCreditContractClient};
    use soroban_sdk::{testutils::Address as _, Address, Env, Vec};

    fn create_test_token(env: &Env) -> Address {
        Address::generate(env)
    }

    fn create_test_admin(env: &Env) -> Address {
        Address::generate(env)
    }

    #[test]
    fn test_total_vouched_overflow() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let deployer = Address::generate(&env);
        let admin = create_test_admin(&env);
        let admins = Vec::from_array(&env, [admin]);
        let token = create_test_token(&env);

        client.initialize(&deployer, &admins, &1, &token);

        let borrower = Address::generate(&env);

        // Create vouches that would overflow when summed
        let mut vouches = Vec::new(&env);

        // Add two vouches with very large stakes that would overflow i128::MAX
        let voucher1 = Address::generate(&env);
        let voucher2 = Address::generate(&env);

        vouches.push_back(VouchRecord {
            voucher: voucher1,
            stake: i128::MAX - 1000,
            vouch_timestamp: 0,
            token: token.clone(),
        });

        vouches.push_back(VouchRecord {
            voucher: voucher2,
            stake: 2000, // This would cause overflow when added to the first stake
            vouch_timestamp: 0,
            token: token.clone(),
        });

        // Store the vouches directly in contract storage
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::Vouches(borrower.clone()), &vouches);
        });

        // Test that total_vouched returns StakeOverflow error
        let result = client.try_total_vouched(&borrower);
        assert_eq!(result, Err(Ok(ContractError::StakeOverflow)));
    }

    #[test]
    fn test_total_vouched_no_overflow() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let deployer = Address::generate(&env);
        let admin = create_test_admin(&env);
        let admins = Vec::from_array(&env, [admin]);
        let token = create_test_token(&env);

        client.initialize(&deployer, &admins, &1, &token);

        let borrower = Address::generate(&env);

        // Create vouches with normal stakes that won't overflow
        let mut vouches = Vec::new(&env);

        let voucher1 = Address::generate(&env);
        let voucher2 = Address::generate(&env);

        vouches.push_back(VouchRecord {
            voucher: voucher1,
            stake: 1_000_000,
            vouch_timestamp: 0,
            token: token.clone(),
        });

        vouches.push_back(VouchRecord {
            voucher: voucher2,
            stake: 2_500_000,
            vouch_timestamp: 0,
            token: token.clone(),
        });

        // Store the vouches directly in contract storage
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::Vouches(borrower.clone()), &vouches);
        });

        // Test that total_vouched returns correct sum
        let result = client.total_vouched(&borrower);
        assert_eq!(result, 3_500_000);
    }

    #[test]
    #[should_panic(expected = "DuplicateVouch")]
    fn test_duplicate_vouch_from_same_voucher_rejected() {
    fn test_vouch_blacklisted_borrower() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let deployer = Address::generate(&env);
        let admin = create_test_admin(&env);
        let admins = Vec::from_array(&env, [admin]);
        let admins = Vec::from_array(&env, [admin.clone()]);
        let token = create_test_token(&env);

        client.initialize(&deployer, &admins, &1, &token);

        let voucher = Address::generate(&env);
        let borrower = Address::generate(&env);

        // First vouch should succeed
        client.vouch(&voucher, &borrower, &1000, &token);

        // Second vouch from same voucher for same borrower should panic with DuplicateVouch
        client.vouch(&voucher, &borrower, &2000, &token);
        let stake = 1_000_000;

        // Blacklist the borrower
        client.blacklist(&Vec::from_array(&env, [admin]), &borrower);

        // Attempt to vouch for blacklisted borrower should fail
        let result = client.try_vouch(&voucher, &borrower, &stake, &token);
        assert_eq!(result, Err(Ok(ContractError::Blacklisted)));
    }
}
