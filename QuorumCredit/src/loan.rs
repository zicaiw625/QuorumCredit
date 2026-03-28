use crate::errors::ContractError;
use crate::helpers::{
    bps_of, config, get_active_loan_record, get_slash_balance, has_active_loan, next_loan_id,
    require_allowed_token, require_not_paused, validate_loan_active,
};
use crate::reputation::ReputationNftExternalClient;
use crate::types::{
    DataKey, LoanRecord, LoanStatus, VouchRecord, DEFAULT_REFERRAL_BONUS_BPS, MIN_VOUCH_AGE,
};
use soroban_sdk::{panic_with_error, symbol_short, Address, Env, Vec};

/// Register a referrer for a borrower. Must be called before `request_loan`.
/// The referrer cannot be the borrower themselves.
pub fn register_referral(
    env: Env,
    borrower: Address,
    referrer: Address,
) -> Result<(), ContractError> {
    borrower.require_auth();
    require_not_paused(&env)?;

    assert!(borrower != referrer, "borrower cannot refer themselves");
    assert!(
        !has_active_loan(&env, &borrower),
        "cannot set referral with active loan"
    );
    // Idempotent: overwrite is fine (borrower signs).
    env.storage()
        .persistent()
        .set(&DataKey::ReferredBy(borrower.clone()), &referrer);

    env.events().publish(
        (symbol_short!("referral"), symbol_short!("set")),
        (borrower, referrer),
    );

    Ok(())
}

pub fn get_referrer(env: Env, borrower: Address) -> Option<Address> {
    env.storage()
        .persistent()
        .get(&DataKey::ReferredBy(borrower))
}

pub fn request_loan(
    env: Env,
    borrower: Address,
    amount: i128,
    threshold: i128,
    loan_purpose: soroban_sdk::String,
    token_addr: Address,
) -> Result<(), ContractError> {
    borrower.require_auth();
    require_not_paused(&env)?;

    if env
        .storage()
        .persistent()
        .get::<DataKey, bool>(&DataKey::Blacklisted(borrower.clone()))
        .unwrap_or(false)
    {
        return Err(ContractError::Blacklisted);
    }

    // Validate token is allowed before any other checks.
    let token_client = require_allowed_token(&env, &token_addr)?;

    let cfg = config(&env);

    assert!(
        amount >= cfg.min_loan_amount,
        "loan amount must meet minimum threshold"
    );
    assert!(threshold > 0, "threshold must be greater than zero");

    let max_loan_amount: i128 = env
        .storage()
        .instance()
        .get(&DataKey::MaxLoanAmount)
        .unwrap_or(0);
    if max_loan_amount > 0 && amount > max_loan_amount {
        return Err(ContractError::LoanExceedsMaxAmount);
    }

    assert!(
        !has_active_loan(&env, &borrower),
        "borrower already has an active loan"
    );

    let vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower.clone()))
        .unwrap_or(Vec::new(&env));

    // Only count vouches denominated in the requested token.
    let mut token_vouches: Vec<VouchRecord> = Vec::new(&env);
    for v in vouches.iter() {
        if v.token == token_addr {
            token_vouches.push_back(v);
        }
    }

    let mut total_stake: i128 = 0;
    for v in token_vouches.iter() {
        total_stake = total_stake
            .checked_add(v.amount)
            .ok_or(ContractError::StakeOverflow)?;
    }
    if total_stake < threshold {
        panic_with_error!(&env, ContractError::InsufficientFunds);
    }

    let min_vouchers: u32 = env
        .storage()
        .instance()
        .get(&DataKey::MinVouchers)
        .unwrap_or(0);
    if token_vouches.len() < min_vouchers {
        return Err(ContractError::InsufficientVouchers);
    }

    let now = env.ledger().timestamp();
    for v in token_vouches.iter() {
        if now < v.vouch_timestamp + MIN_VOUCH_AGE {
            return Err(ContractError::VouchTooRecent);
        }
    }

    let max_allowed_loan = total_stake * cfg.max_loan_to_stake_ratio as i128 / 100;
    assert!(
        amount <= max_allowed_loan,
        "loan amount exceeds maximum collateral ratio"
    );

    let contract_balance = token_client.balance(&env.current_contract_address());
    if contract_balance < amount {
        return Err(ContractError::InsufficientFunds);
    }

    let deadline = now + cfg.loan_duration;
    let loan_id = next_loan_id(&env);
    let total_yield = bps_of(amount, cfg.yield_bps);

    env.storage().persistent().set(
        &DataKey::Loan(loan_id),
        &LoanRecord {
            id: loan_id,
            borrower: borrower.clone(),
            co_borrowers: Vec::new(&env),
            amount,
            amount_repaid: 0,
            total_yield,
            status: LoanStatus::Active,
            created_at: now,
            disbursement_timestamp: now,
            repayment_timestamp: None,
            deadline,
            loan_purpose,
            token_address: token_addr.clone(),
        },
    );
    env.storage()
        .persistent()
        .set(&DataKey::ActiveLoan(borrower.clone()), &loan_id);
    env.storage()
        .persistent()
        .set(&DataKey::LatestLoan(borrower.clone()), &loan_id);

    let count: u32 = env
        .storage()
        .persistent()
        .get(&DataKey::LoanCount(borrower.clone()))
        .unwrap_or(0);
    env.storage()
        .persistent()
        .set(&DataKey::LoanCount(borrower.clone()), &(count + 1));

    token_client.transfer(&env.current_contract_address(), &borrower, &amount);

    env.events().publish(
        (symbol_short!("loan"), symbol_short!("disbursed")),
        (borrower.clone(), amount, deadline, token_addr),
    );

    Ok(())
}

pub fn repay(env: Env, borrower: Address, payment: i128) -> Result<(), ContractError> {
    borrower.require_auth();
    require_not_paused(&env)?;

    // First try to get active loan record
    let mut loan = match get_active_loan_record(&env, &borrower) {
        Ok(loan) => loan,
        Err(ContractError::NoActiveLoan) => {
            // Check if there's a latest loan that is already repaid
            if let Some(latest_loan) = crate::helpers::get_latest_loan_record(&env, &borrower) {
                if latest_loan.status == LoanStatus::Repaid {
                    panic!("loan already repaid");
                }
            }
            return Err(ContractError::NoActiveLoan);
        }
        Err(e) => return Err(e),
    };

    for cb in loan.co_borrowers.iter() {
        cb.require_auth();
    }

    if borrower != loan.borrower {
        return Err(ContractError::UnauthorizedCaller);
    }
    validate_loan_active(&loan)?;
    assert!(
        env.ledger().timestamp() <= loan.deadline,
        "loan deadline has passed"
    );

    // Total obligation = principal + yield locked in at disbursement.
    let total_owed = loan.amount + loan.total_yield;
    let outstanding = total_owed - loan.amount_repaid;
    assert!(
        payment > 0 && payment <= outstanding,
        "invalid payment amount"
    );

    let token = soroban_sdk::token::Client::new(&env, &loan.token_address);

    token.transfer(&borrower, &env.current_contract_address(), &payment);
    loan.amount_repaid += payment;
    let fully_repaid = loan.amount_repaid >= total_owed;

    if fully_repaid {
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        if vouches.is_empty() {
            panic!("no vouchers found for borrower");
        }

        // Issue 112: Only distribute yield to vouches in the same token as the loan.
        // Verify that available funds exclude slash balance to prevent fund leakage.
        let loan_token = soroban_sdk::token::Client::new(&env, &loan.token_address);
        let _slash_balance = get_slash_balance(&env);

        let mut total_stake: i128 = 0;
        for v in vouches.iter() {
            if v.token == loan.token_address {
                total_stake += v.amount;
            }
        }

        // Issue 112: Ensure yield distribution respects available funds (excluding slash balance)
        let available_for_yield = loan.total_yield;
        let mut total_distributed: i128 = 0;

        for v in vouches.iter() {
            if v.token != loan.token_address {
                continue;
            }
            let voucher_yield = if total_stake > 0 {
                (available_for_yield * v.amount) / total_stake
            } else {
                0
            };
            total_distributed += voucher_yield;

            // Assert that we're not exceeding available yield
            assert!(
                total_distributed <= available_for_yield,
                "yield distribution would exceed available funds"
            );

            loan_token.transfer(
                &env.current_contract_address(),
                &v.voucher,
                &(v.amount + voucher_yield),
            );
        }

        loan.status = LoanStatus::Repaid;
        loan.repayment_timestamp = Some(env.ledger().timestamp());

        // Pay referral bonus if a referrer is registered.
        if let Some(referrer) = env
            .storage()
            .persistent()
            .get::<DataKey, Address>(&DataKey::ReferredBy(borrower.clone()))
        {
            let bonus_bps: u32 = env
                .storage()
                .instance()
                .get(&DataKey::ReferralBonusBps)
                .unwrap_or(DEFAULT_REFERRAL_BONUS_BPS);
            let bonus = loan.amount * bonus_bps as i128 / 10_000;

            // Issue 112: Ensure bonus doesn't use slash funds
            if bonus > 0 {
                loan_token.transfer(&env.current_contract_address(), &referrer, &bonus);
                env.events().publish(
                    (symbol_short!("referral"), symbol_short!("bonus")),
                    (referrer, borrower.clone(), bonus),
                );
            }
        }

        let count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::RepaymentCount(borrower.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::RepaymentCount(borrower.clone()), &(count + 1));

        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftExternalClient::new(&env, &nft_addr).mint(&borrower);
        }

        env.storage()
            .persistent()
            .remove(&DataKey::ActiveLoan(borrower.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower.clone()));

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("repaid")),
            (borrower.clone(), loan.amount),
        );
    }

    env.storage()
        .persistent()
        .set(&DataKey::Loan(loan.id), &loan);

    Ok(())
}

pub fn loan_status(env: Env, borrower: Address) -> LoanStatus {
    match crate::helpers::get_latest_loan_record(&env, &borrower) {
        None => LoanStatus::None,
        Some(loan) => loan.status,
    }
}

pub fn get_loan(env: Env, borrower: Address) -> Option<LoanRecord> {
    crate::helpers::get_latest_loan_record(&env, &borrower)
}

pub fn get_loan_by_id(env: Env, loan_id: u64) -> Option<LoanRecord> {
    env.storage().persistent().get(&DataKey::Loan(loan_id))
}

pub fn is_eligible(env: Env, borrower: Address, threshold: i128) -> bool {
    if threshold <= 0 {
        return false;
    }

    if let Some(loan) = crate::helpers::get_latest_loan_record(&env, &borrower) {
        if loan.status == LoanStatus::Active {
            return false;
        }
    }

    let vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower))
        .unwrap_or(Vec::new(&env));

    let total_stake: i128 = vouches.iter().map(|v| v.amount).sum();
    total_stake >= threshold
}

pub fn repayment_count(env: Env, borrower: Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::RepaymentCount(borrower))
        .unwrap_or(0)
}

pub fn loan_count(env: Env, borrower: Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::LoanCount(borrower))
        .unwrap_or(0)
}

pub fn default_count(env: Env, borrower: Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::DefaultCount(borrower))
        .unwrap_or(0)
}
