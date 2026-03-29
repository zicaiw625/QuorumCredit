use crate::errors::ContractError;
use crate::types::{Config, DataKey, LoanRecord};
use soroban_sdk::{token, Address, Env, String, Vec};

/// Returns true if the address is the all-zeros account or contract address.
pub fn is_zero_address(env: &Env, addr: &Address) -> bool {
    // Stellar zero account: all-zero 32-byte ed25519 key
    let zero_account = Address::from_string(&String::from_str(
        env,
        "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
    ));
    // Stellar zero contract: all-zero 32-byte contract hash
    let zero_contract = Address::from_string(&String::from_str(
        env,
        "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
    ));
    addr == &zero_account || addr == &zero_contract
}

pub fn require_not_paused(env: &Env) -> Result<(), ContractError> {
    let paused: bool = env
        .storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false);
    if paused {
        Err(ContractError::ContractPaused)
    } else {
        Ok(())
    }
}

/// Returns `Err(InsufficientFunds)` if `amount` is not strictly positive (≤ 0).
/// Use this for all numeric inputs that must be > 0 (stakes, loan amounts, thresholds).
pub fn require_positive_amount(_env: &Env, amount: i128) -> Result<(), ContractError> {
    if amount <= 0 {
        return Err(ContractError::InsufficientFunds);
    }
    Ok(())
}

pub fn config(env: &Env) -> Config {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .expect("not initialized")
}

pub fn add_slash_balance(env: &Env, amount: i128) {
    let current: i128 = env
        .storage()
        .instance()
        .get(&DataKey::SlashTreasury)
        .unwrap_or(0);
    env.storage()
        .instance()
        .set(&DataKey::SlashTreasury, &(current + amount));
}

pub fn has_active_loan(env: &Env, borrower: &Address) -> bool {
    matches!(get_active_loan_record(env, borrower), Ok(loan) if loan.status == crate::types::LoanStatus::Active)
}

pub fn next_loan_id(env: &Env) -> u64 {
    let loan_id = env
        .storage()
        .instance()
        .get(&DataKey::LoanCounter)
        .unwrap_or(0u64)
        .checked_add(1)
        .expect("loan ID overflow");
    env.storage()
        .instance()
        .set(&DataKey::LoanCounter, &loan_id);
    loan_id
}

pub fn get_active_loan_record(env: &Env, borrower: &Address) -> Result<LoanRecord, ContractError> {
    let loan_id: u64 = env
        .storage()
        .persistent()
        .get(&DataKey::ActiveLoan(borrower.clone()))
        .ok_or(ContractError::NoActiveLoan)?;
    env.storage()
        .persistent()
        .get(&DataKey::Loan(loan_id))
        .ok_or(ContractError::NoActiveLoan)
}

pub fn get_latest_loan_record(env: &Env, borrower: &Address) -> Option<LoanRecord> {
    let loan_id: u64 = env
        .storage()
        .persistent()
        .get(&DataKey::LatestLoan(borrower.clone()))?;
    env.storage().persistent().get(&DataKey::Loan(loan_id))
}

pub fn token(env: &Env) -> token::Client<'_> {
    let addr = config(env).token;
    token::Client::new(env, &addr)
}

/// Returns a token client for `addr` after verifying it is an allowed token
/// (either the primary protocol token or in `Config.allowed_tokens`).
pub fn require_allowed_token<'a>(
    env: &'a Env,
    addr: &Address,
) -> Result<token::Client<'a>, ContractError> {
    let cfg = config(env);
    if *addr == cfg.token || cfg.allowed_tokens.iter().any(|t| t == *addr) {
        Ok(token::Client::new(env, addr))
    } else {
        Err(ContractError::InvalidToken)
    }
}

pub fn require_admin_approval(env: &Env, admin_signers: &Vec<Address>) {
    let config = config(env);
    assert!(
        admin_signers.len() >= config.admin_threshold,
        "insufficient admin approvals"
    );
    for signer in admin_signers.iter() {
        assert!(
            config.admins.iter().any(|a| a == signer),
            "signer is not a registered admin"
        );
        signer.require_auth();
    }
}

/// Validates that an address is not a zero address
pub fn require_valid_address(env: &Env, addr: &Address) -> Result<(), ContractError> {
    if is_zero_address(env, addr) {
        Err(ContractError::ZeroAddress)
    } else {
        Ok(())
    }
}

/// Validates that an address implements the SEP-41 token interface by attempting
/// to call `balance()` on it. A plain account address will cause a host trap,
/// which we catch via `try_invoke` semantics using the token client's try_ variant.
pub fn require_valid_token(env: &Env, addr: &Address) -> Result<(), ContractError> {
    require_valid_address(env, addr)?;
    // Attempt to call balance() on the address. If it's not a token contract,
    // the invocation will fail and we return InvalidToken.
    let client = token::Client::new(env, addr);
    // Use a dummy address (the contract itself) — we only care whether the call
    // succeeds, not the returned value.
    let probe = env.current_contract_address();
    if client.try_balance(&probe).is_err() {
        return Err(ContractError::InvalidToken);
    }
    Ok(())
}

pub fn validate_admin_config(
    env: &Env,
    admins: &Vec<Address>,
    admin_threshold: u32,
) -> Result<(), ContractError> {
    assert!(!admins.is_empty(), "at least one admin is required");
    assert!(
        admin_threshold > 0,
        "admin threshold must be greater than zero"
    );
    assert!(
        admin_threshold <= admins.len(),
        "admin threshold cannot exceed admin count"
    );

    let admin_count = admins.len();
    for i in 0..admin_count {
        let admin = admins.get(i).unwrap();

        // Validate admin address is not zero
        require_valid_address(env, &admin)?;

        // Check for duplicates
        for j in 0..i {
            let prior_admin = admins.get(j).unwrap();
            assert!(admin != prior_admin, "duplicate admin");
        }
    }

    Ok(())
}

/// Compute basis points: amount * bps / 10_000
pub fn bps_of(amount: i128, bps: u32) -> i128 {
    amount * bps as i128 / 10_000
}
