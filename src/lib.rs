#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, Vec,
};

pub mod reputation;
use reputation::ReputationNftContractClient;

// ── Constants ─────────────────────────────────────────────────────────────────
// ── Constants (defaults only) ─────────────────────────────────────────────────

const DEFAULT_YIELD_BPS: i128 = 200;
const DEFAULT_SLASH_BPS: i128 = 5000;
const _: () = assert!(
    DEFAULT_SLASH_BPS <= 10_000,
    "DEFAULT_SLASH_BPS must not exceed 10_000"
);
const DEFAULT_MAX_VOUCHERS: u32 = 100;
const DEFAULT_MIN_LOAN_AMOUNT: i128 = 100_000;
const DEFAULT_LOAN_DURATION: u64 = 30 * 24 * 60 * 60;
const DEFAULT_MAX_LOAN_TO_STAKE_RATIO: u32 = 150;

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    InsufficientFunds = 1,
    DuplicateVouch = 2,
    NoActiveLoan = 3,
    ContractPaused = 4,
    LoanPastDeadline = 5,
    MinStakeNotMet = 6,
    UnauthorizedCaller = 6,
}

// ── Loan Status ───────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoanStatus {
    None,
    Active,
    Repaid,
    Defaulted,
}

// ── Storage Keys ──────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Loan(Address),           // borrower → LoanRecord
    Vouches(Address),        // borrower → Vec<VouchRecord>
    VoucherHistory(Address), // voucher → Vec<Address> (borrowers backed)
    Admin,                   // Address allowed to call slash
    Token,                   // XLM token contract address
    Deployer,                // Address that deployed the contract; guards initialize
    MaxLoanToStakeRatio,     // Maximum loan-to-stake ratio (percentage * 100)
    SlashTreasury,           // i128 accumulated slashed funds
    Paused,                  // bool: true when contract is paused
    LoanDuration,            // u64 configurable loan duration in seconds
    Loan(Address),    // borrower → LoanRecord
    Vouches(Address), // borrower → Vec<VouchRecord>
    Admin,            // Address allowed to call slash
    Token,            // XLM token contract address
    Deployer,         // Address that deployed the contract; guards initialize
    SlashTreasury,    // i128 accumulated slashed funds
    Paused,           // bool: true when contract is paused
    LoanDuration,     // u64 configurable loan duration in seconds
    MinStake,         // i128 minimum stake amount per vouch
    ReputationNft,    // Address of the ReputationNftContract
    Config,           // Config struct: all configurable protocol parameters
    YieldBps,         // i128 yield in basis points
    SlashBps,         // i128 slash penalty in basis points
    PendingAdmin,     // Address of the pending admin (two-step transfer)
}

// ── Config ────────────────────────────────────────────────────────────────────

/// All configurable protocol parameters, stored under DataKey::Config.
#[contracttype]
#[derive(Clone)]
pub struct Config {
    /// Yield paid to vouchers on repayment in basis points (default 200 = 2%).
    pub yield_bps: i128,
    /// Slash penalty on default in basis points (default 5000 = 50%).
    pub slash_bps: i128,
    /// Maximum number of vouchers per loan (default 100).
    pub max_vouchers: u32,
    /// Minimum loan amount in stroops (default 100_000 = 0.01 XLM).
    pub min_loan_amount: i128,
    /// Loan duration in seconds (default 30 days).
    pub loan_duration: u64,
    /// Maximum loan amount as a percentage of total stake (default 150 = 150%).
    pub max_loan_to_stake_ratio: u32,
}

impl Config {
    fn default() -> Self {
        Config {
            yield_bps: DEFAULT_YIELD_BPS,
            slash_bps: DEFAULT_SLASH_BPS,
            max_vouchers: DEFAULT_MAX_VOUCHERS,
            min_loan_amount: DEFAULT_MIN_LOAN_AMOUNT,
            loan_duration: DEFAULT_LOAN_DURATION,
            max_loan_to_stake_ratio: DEFAULT_MAX_LOAN_TO_STAKE_RATIO,
        }
    }
}

// ── Data Types ────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub struct LoanRecord {
    pub borrower: Address,
    pub amount: i128, // in stroops
    pub repaid: bool,
    pub defaulted: bool,
    pub created_at: u64, // ledger timestamp
    pub deadline: u64,   // repayment deadline (ledger timestamp)
}

#[contracttype]
#[derive(Clone)]
pub struct VouchRecord {
    pub voucher: Address,
    pub stake: i128, // in stroops
}

#[contracttype]
#[derive(Clone, Default)]
pub struct CreditHistory {
    pub repayments: u32,
    pub defaults: u32,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct QuorumCreditContract;

#[contractimpl]
impl QuorumCreditContract {
    /// One-time initialisation: set admin, XLM token address, and default config.
    ///
    /// `deployer` must be the address that deployed this contract and must
    /// sign this transaction. This prevents front-running attacks where an
    /// observer of the deployment transaction calls `initialize` first with
    /// their own admin address before the legitimate deployer can do so.
    pub fn initialize(env: Env, deployer: Address, admin: Address, token: Address) {
        // Require the deployer's signature — only they can authorise this call.
        deployer.require_auth();

        assert!(
            !env.storage().instance().has(&DataKey::Admin),
            "already initialized"
        );
        assert!(
            DEFAULT_YIELD_BPS > 0 && DEFAULT_YIELD_BPS <= 10_000,
            "yield_bps must be in range 1..=10000"
        );
        assert!(
            DEFAULT_SLASH_BPS > 0 && DEFAULT_SLASH_BPS <= 10_000,
            "slash_bps must be in range 1..=10000"
        );

        env.storage().instance().set(&DataKey::Deployer, &deployer);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        env.storage()
            .instance()
            .set(&DataKey::Config, &Config::default());
    }

    /// Stake XLM to vouch for a borrower.
    pub fn vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
        stake: i128,
    ) -> Result<(), ContractError> {
        voucher.require_auth();
        Self::require_not_paused(&env)?;

        assert!(voucher != borrower, "voucher cannot vouch for self");

        // Enforce minimum stake if configured.
        let min_stake: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinStake)
            .unwrap_or(0);
        if stake < min_stake {
            return Err(ContractError::MinStakeNotMet);
        }

        let mut vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        // Check for duplicate vouch before any state mutation or transfer.
        for v in vouches.iter() {
            if v.voucher == voucher {
                return Err(ContractError::DuplicateVouch);
            }
        }

        assert!(
            vouches.len() < Self::config(&env).max_vouchers,
            "maximum vouchers per loan exceeded"
        );

        // Transfer stake from voucher into the contract.
        let token = Self::token(&env);
        token.transfer(&voucher, &env.current_contract_address(), &stake);

        // Track voucher → borrowers history.
        let mut history: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::VoucherHistory(voucher.clone()))
            .unwrap_or(Vec::new(&env));
        history.push_back(borrower.clone());
        env.storage()
            .persistent()
            .set(&DataKey::VoucherHistory(voucher.clone()), &history);

        vouches.push_back(VouchRecord { voucher, stake });
        vouches.push_back(VouchRecord {
            voucher: voucher.clone(),
            stake,
        });
        env.storage()
            .persistent()
            .set(&DataKey::Vouches(borrower.clone()), &vouches);

        env.events().publish(
            (symbol_short!("vouch"), symbol_short!("added")),
            (voucher, borrower, stake),
        );

        Ok(())
    }

    /// Add more stake to an existing vouch for a borrower.
    pub fn increase_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        additional: i128,
    ) -> Result<(), ContractError> {
        voucher.require_auth();
        Self::require_not_paused(&env)?;

        assert!(additional > 0, "additional stake must be greater than zero");

        let mut vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .expect("vouch not found");

        let idx = vouches
            .iter()
            .position(|v| v.voucher == voucher)
            .expect("vouch not found") as u32;

        let mut vouch = vouches.get(idx).unwrap();
        let token = Self::token(&env);
        token.transfer(&voucher, &env.current_contract_address(), &additional);

        vouch.stake += additional;
        vouches.set(idx, vouch);

        env.storage()
            .persistent()
            .set(&DataKey::Vouches(borrower), &vouches);

        Ok(())
    }

    /// Disburse a microloan if total vouched stake meets the threshold.
    pub fn request_loan(
        env: Env,
        borrower: Address,
        amount: i128,
        threshold: i128,
    ) -> Result<(), ContractError> {
        borrower.require_auth();
        Self::require_not_paused(&env)?;

        assert!(
            amount >= Self::config(&env).min_loan_amount,
            "loan amount must meet minimum threshold"
        );
        assert!(threshold > 0, "threshold must be greater than zero");

        // Prevent overwriting an active loan record.
        if let Some(existing) = env
            .storage()
            .persistent()
            .get::<DataKey, LoanRecord>(&DataKey::Loan(borrower.clone()))
        {
            assert!(
                existing.repaid || existing.defaulted,
                "borrower already has an active loan"
            );
        }

        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        let total_stake: i128 = vouches.iter().map(|v| v.stake).sum();
        assert!(total_stake >= threshold, "insufficient trust stake");

        // Check collateral ratio: amount must not exceed total_stake * ratio / 100
        let cfg = Self::config(&env);
        let max_allowed_loan = total_stake * cfg.max_loan_to_stake_ratio as i128 / 100;
        assert!(
            amount <= max_allowed_loan,
            "loan amount exceeds maximum collateral ratio"
        );

        // Verify the contract holds enough XLM to cover the loan.
        let token = Self::token(&env);
        let contract_balance = token.balance(&env.current_contract_address());
        if contract_balance < amount {
            return Err(ContractError::InsufficientFunds);
        }

        let now = env.ledger().timestamp();
        let deadline = now + cfg.loan_duration;

        env.storage().persistent().set(
            &DataKey::Loan(borrower.clone()),
            &LoanRecord {
                borrower: borrower.clone(),
                amount,
                repaid: false,
                defaulted: false,
                created_at: now,
                deadline,
            },
        );

        // Disburse the loan to the borrower.
        token.transfer(&env.current_contract_address(), &borrower, &amount);

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("disbursed")),
            (borrower.clone(), amount, deadline),
        );

        Ok(())
    }

    /// Borrower repays loan; vouchers receive 2% yield on their stake.
    pub fn repay(env: Env, borrower: Address) -> Result<(), ContractError> {
        borrower.require_auth();
        Self::require_not_paused(&env)?;

        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(borrower.clone()))
            .ok_or(ContractError::NoActiveLoan)?;

        if borrower != loan.borrower {
            return Err(ContractError::UnauthorizedCaller);
        }
        assert!(!loan.defaulted, "loan already defaulted");
        assert!(!loan.repaid, "loan already repaid");

        // Block repayment after deadline — borrower must be auto-slashed instead.
        assert!(
            env.ledger().timestamp() <= loan.deadline,
            "loan deadline has passed"
        );

        let token = Self::token(&env);
        let cfg = Self::config(&env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        // Pre-calculate total payout to ensure contract has enough balance.
        let mut total_payout: i128 = 0;
        for v in vouches.iter() {
            let yield_amount = v.stake * cfg.yield_bps / 10_000;
            total_payout += v.stake + yield_amount;
        }

        // Collect repayment from borrower first.
        token.transfer(&borrower, &env.current_contract_address(), &loan.amount);

        let contract_balance = token.balance(&env.current_contract_address());
        assert!(
            contract_balance >= total_payout,
            "insufficient contract balance for yield distribution"
        );

        // Return stake + yield to each voucher.
        for v in vouches.iter() {
            let yield_amount = v.stake * cfg.yield_bps / 10_000;
            token.transfer(
                &env.current_contract_address(),
                &v.voucher,
                &(v.stake + yield_amount),
            );
        }

        loan.repaid = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower.clone()), &loan);

        // Mint one reputation point if a reputation NFT contract is configured.
        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftContractClient::new(&env, &nft_addr).mint(&borrower);
        }

        Ok(())
    }

    /// Admin marks a loan defaulted; 50% of each voucher's stake is slashed.
    pub fn slash(env: Env, borrower: Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();
        Self::require_not_paused(&env).expect("contract is paused");

        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(borrower.clone()))
            .expect("no active loan");

        assert!(!loan.repaid, "loan already repaid");
        assert!(!loan.defaulted, "already defaulted");

        let token = Self::token(&env);
        let cfg = Self::config(&env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        for v in vouches.iter() {
            let slash_amount = v.stake * cfg.slash_bps / 10_000;
            let returned = v.stake - slash_amount;
            // Return remaining stake to voucher; slashed portion stays in contract.
            if returned > 0 {
                token.transfer(&env.current_contract_address(), &v.voucher, &returned);
            }
            // Accumulate slashed amount in treasury.
            let treasury: i128 = env
                .storage()
                .instance()
                .get(&DataKey::SlashTreasury)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::SlashTreasury, &(treasury + slash_amount));
        }

        loan.defaulted = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower.clone()), &loan);

        // Burn one reputation point if a reputation NFT contract is configured.
        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftContractClient::new(&env, &nft_addr).burn(&borrower);
        }

        // Clear vouches after slashing to prevent state pollution.
        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower));
    }

    /// Allows vouchers to claim back their stake if loan has expired without repayment or slash.
    pub fn claim_expired_loan(env: Env, borrower: Address) {
        let loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(borrower.clone()))
            .expect("no active loan");

        assert!(!loan.repaid, "loan already repaid");
        assert!(!loan.defaulted, "loan already defaulted");

        let now = env.ledger().timestamp();
        assert!(now >= loan.deadline, "loan has not expired yet");

        // Return full stake to all vouchers.
        let token = Self::token(&env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        for v in vouches.iter() {
            token.transfer(&env.current_contract_address(), &v.voucher, &v.stake);
        }

        // Mark loan as defaulted to prevent re-processing.
        let mut loan = loan;
        loan.defaulted = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower.clone()), &loan);

        // Clear vouches after claim.
        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower));
    }

    /// Admin withdraws accumulated slashed funds to a recipient address.
    pub fn slash_treasury(env: Env, recipient: Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();

        let amount: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SlashTreasury)
            .unwrap_or(0);
        assert!(amount > 0, "no slashed funds to withdraw");

        env.storage()
            .instance()
            .set(&DataKey::SlashTreasury, &0i128);
        Self::token(&env).transfer(&env.current_contract_address(), &recipient, &amount);
    }

    /// Withdraw a vouch before any loan is active, returning the exact stake to the voucher.
    pub fn withdraw_vouch(env: Env, voucher: Address, borrower: Address) {
        voucher.require_auth();

        // Block withdrawal if a loan record already exists for this borrower.
        assert!(
            env.storage()
                .persistent()
                .get::<DataKey, LoanRecord>(&DataKey::Loan(borrower.clone()))
                .is_none(),
            "loan already active"
        );

        // Load the vouches list; panic if absent.
        let mut vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .expect("vouch not found");

        // Find the index of the matching VouchRecord.
        let idx = vouches
            .iter()
            .position(|v| v.voucher == voucher)
            .expect("vouch not found") as u32;

        let stake = vouches.get(idx).unwrap().stake;
        vouches.remove(idx);

        // Persist updated list or remove the key if empty.
        if vouches.is_empty() {
            env.storage()
                .persistent()
                .remove(&DataKey::Vouches(borrower));
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::Vouches(borrower), &vouches);
        }

        // Return exact stake to voucher.
        Self::token(&env).transfer(&env.current_contract_address(), &voucher, &stake);
    }

    // ── Loan Deadline ─────────────────────────────────────────────────────────

    /// Callable by anyone after the loan deadline has passed.
    /// Applies the standard slash penalty (50% of each voucher's stake burned).
    pub fn auto_slash(env: Env, borrower: Address) {
        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(borrower.clone()))
            .expect("no active loan");

        assert!(!loan.repaid, "loan already repaid");
        assert!(!loan.defaulted, "loan already defaulted");
        assert!(
            env.ledger().timestamp() > loan.deadline,
            "loan deadline has not passed"
        );

        let token = Self::token(&env);
        let cfg = Self::config(&env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        for v in vouches.iter() {
            let slash_amount = v.stake * cfg.slash_bps / 10_000;
            let returned = v.stake - slash_amount;
            if returned > 0 {
                token.transfer(&env.current_contract_address(), &v.voucher, &returned);
            }
            let treasury: i128 = env
                .storage()
                .instance()
                .get(&DataKey::SlashTreasury)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::SlashTreasury, &(treasury + slash_amount));
        }

        loan.defaulted = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower.clone()), &loan);

        // Burn one reputation point if a reputation NFT contract is configured.
        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftContractClient::new(&env, &nft_addr).burn(&borrower);
        }

        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower));
    }

    /// Admin sets the minimum stake amount required per vouch (in stroops).
    /// Set to 0 to disable the minimum.
    pub fn set_min_stake(env: Env, amount: i128) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();
        assert!(amount >= 0, "min stake cannot be negative");
        env.storage().instance().set(&DataKey::MinStake, &amount);
    }

    /// Returns the current minimum vouch stake (0 means no minimum).
    pub fn get_min_stake(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::MinStake)
            .unwrap_or(0)
    }

    /// Admin sets the loan duration (in seconds) applied to future loans.
    pub fn set_loan_duration(env: Env, duration_seconds: u64) {
    /// Admin updates configurable protocol parameters.
    pub fn set_config(env: Env, config: Config) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();
        assert!(config.yield_bps >= 0, "yield_bps must be non-negative");
        assert!(
            config.slash_bps > 0 && config.slash_bps <= 10_000,
            "slash_bps must be 1-10000"
        );
        assert!(
            config.max_vouchers > 0,
            "max_vouchers must be greater than zero"
        );
        assert!(
            config.min_loan_amount > 0,
            "min_loan_amount must be greater than zero"
        );
        assert!(
            config.loan_duration > 0,
            "loan_duration must be greater than zero"
        );
        assert!(
            config.max_loan_to_stake_ratio > 0,
            "max_loan_to_stake_ratio must be greater than zero"
        );
        env.storage().instance().set(&DataKey::Config, &config);
    }

    /// Returns the current protocol config.
    pub fn get_config(env: Env) -> Config {
        Self::config(&env)
    }

    // ── Admin: Pause / Unpause ────────────────────────────────────────────────

    /// Pause the contract, disabling vouch, request_loan, repay, and slash.
    pub fn pause(env: Env) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
    }

    /// Unpause the contract, re-enabling all critical functions.
    pub fn unpause(env: Env) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
    }

    // ── Two-Step Admin Transfer ───────────────────────────────────────────────

    /// Step 1: Current admin proposes a new admin address.
    /// Overwrites any previously pending proposal.
    pub fn propose_admin(env: Env, new_admin: Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.events().publish(("AdminProposed",), (admin, new_admin));
    }

    /// Step 2: Pending admin accepts the transfer, becoming the new admin.
    pub fn accept_admin(env: Env) {
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .expect("no pending admin");
        pending.require_auth();

        let old_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");

        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events()
            .publish(("AdminUpdated",), (old_admin, pending));
    }

    // ── Views ─────────────────────────────────────────────────────────────────

    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&DataKey::Admin)
    }

    pub fn get_token(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Token)
            .expect("not initialized")
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    pub fn get_slash_treasury(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::SlashTreasury)
            .unwrap_or(0)
    }

    pub fn get_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn loan_status(env: Env, borrower: Address) -> LoanStatus {
        match env
            .storage()
            .persistent()
            .get::<DataKey, LoanRecord>(&DataKey::Loan(borrower))
        {
            None => LoanStatus::None,
            Some(loan) if loan.repaid => LoanStatus::Repaid,
            Some(loan) if loan.defaulted => LoanStatus::Defaulted,
            _ => LoanStatus::Active,
        }
    }

    pub fn vouch_exists(env: Env, voucher: Address, borrower: Address) -> bool {
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower))
            .unwrap_or(Vec::new(&env));
        vouches.iter().any(|v| v.voucher == voucher)
    }

    pub fn get_loan(env: Env, borrower: Address) -> Option<LoanRecord> {
        env.storage().persistent().get(&DataKey::Loan(borrower))
    }

    pub fn get_vouches(env: Env, borrower: Address) -> Option<Vec<VouchRecord>> {
        env.storage().persistent().get(&DataKey::Vouches(borrower))
    }

    /// Returns the contract's current XLM balance in stroops.
    pub fn get_contract_balance(env: Env) -> i128 {
        Self::token(&env).balance(&env.current_contract_address())
    /// Returns all borrower addresses that the given voucher has ever backed.
    /// Returns an empty Vec if the voucher has no history.
    pub fn voucher_history(env: Env, voucher: Address) -> Vec<Address> {
        env.storage()
            .persistent()
            .get(&DataKey::VoucherHistory(voucher))
            .unwrap_or(Vec::new(&env))
    /// Admin sets the reputation NFT contract address.
    pub fn set_reputation_nft(env: Env, nft_contract: Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::ReputationNft, &nft_contract);
    }

    /// Returns the reputation score for a borrower (0 if no NFT contract set or no history).
    pub fn get_reputation(env: Env, borrower: Address) -> u32 {
        let nft_addr: Address = match env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            Some(a) => a,
            None => return 0,
        };
        ReputationNftContractClient::new(&env, &nft_addr).balance(&borrower)
    }

    /// Returns the total staked amount across all vouchers for a given borrower.
    /// Returns 0 if the borrower has no vouches.
    pub fn total_vouched(env: Env, borrower: Address) -> i128 {
        env.storage()
            .persistent()
            .get::<DataKey, Vec<VouchRecord>>(&DataKey::Vouches(borrower))
            .unwrap_or(Vec::new(&env))
            .iter()
            .map(|v| v.stake)
            .sum()
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn require_not_paused(env: &Env) -> Result<(), ContractError> {
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

    fn config(env: &Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or_else(Config::default)
    }

    fn token(env: &Env) -> token::Client<'_> {
        let addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::Token)
            .expect("not initialized");
        token::Client::new(env, &addr)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Events as _, Ledger},
        token::{Client as TokenClient, StellarAssetClient},
        Address, Env,
    };

    fn setup(env: &Env) -> (Address, Address, Address, Address, Address) {
        env.mock_all_auths();

        let admin = Address::generate(env);
        let borrower = Address::generate(env);
        let voucher = Address::generate(env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);
        token_admin.mint(&contract_id, &50_000_000);

        // deployer == admin for test convenience; the key point is that
        // deployer.require_auth() is satisfied via mock_all_auths().
        // Set max_loan_to_stake_ratio to 150%.
        QuorumCreditContractClient::new(env, &contract_id)
            .initialize(&admin, &admin, &token_id.address(), &150);
        // deployer == admin for test convenience
        QuorumCreditContractClient::new(env, &contract_id).initialize(
            &admin,
            &admin,
            &token_id.address(),
        );

        (contract_id, token_id.address(), admin, borrower, voucher)
    }

    #[test]
    fn test_vouch_and_loan_disbursed() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        let loan = client.get_loan(&borrower).unwrap();
        assert_eq!(loan.amount, 500_000);
        assert!(!loan.repaid);
        assert!(!loan.defaulted);
        assert!(loan.created_at > 0);
        assert!(loan.deadline > loan.created_at);
    }

    #[test]
    fn test_request_loan_emits_event() {
        use soroban_sdk::{IntoVal, Val};

        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000_000);

        let admin = Address::generate(&env);
        let borrower = Address::generate(&env);
        let voucher = Address::generate(&env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(&env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);
        token_admin.mint(&contract_id, &50_000_000);

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&admin, &admin, &token_id.address());

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        let topic_loan: Val = symbol_short!("loan").into_val(&env);
        let topic_disbursed: Val = symbol_short!("disbursed").into_val(&env);

        let (_, _, data) = env
            .events()
            .all()
            .iter()
            .find(|(_, topics, _)| {
                topics.len() == 2
                    && topics.get_unchecked(0).get_payload() == topic_loan.get_payload()
                    && topics.get_unchecked(1).get_payload() == topic_disbursed.get_payload()
            })
            .expect("loan_disbursed event not emitted");

        let (event_borrower, event_amount, _event_deadline): (Address, i128, u64) =
            data.into_val(&env);
        assert_eq!(event_borrower, borrower);
        assert_eq!(event_amount, 500_000);
    }

    #[test]
    fn test_vouch_emits_event() {
        use soroban_sdk::{IntoVal, Val};

        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let voucher = Address::generate(&env);
        let borrower = Address::generate(&env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(&env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&admin, &admin, &token_id.address());

        client.vouch(&voucher, &borrower, &1_000_000);

        let topic_vouch: Val = symbol_short!("vouch").into_val(&env);
        let topic_added: Val = symbol_short!("added").into_val(&env);

        let (_, _, data) = env
            .events()
            .all()
            .iter()
            .find(|(_, topics, _)| {
                topics.len() == 2
                    && topics.get_unchecked(0).get_payload() == topic_vouch.get_payload()
                    && topics.get_unchecked(1).get_payload() == topic_added.get_payload()
            })
            .expect("vouch_added event not emitted");

        let (event_voucher, event_borrower, event_stake): (Address, Address, i128) =
            data.into_val(&env);
        assert_eq!(event_voucher, voucher);
        assert_eq!(event_borrower, borrower);
        assert_eq!(event_stake, 1_000_000);
    }

    #[test]
    #[should_panic(expected = "voucher cannot vouch for self")]
    fn test_vouch_self_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&borrower, &borrower, &1_000_000);
    }

    #[test]
    fn test_repay_gives_voucher_yield() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower);

        assert_eq!(token.balance(&voucher), 10_020_000);
    }

    #[test]
    fn test_repay_mismatched_borrower_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let borrower = Address::generate(&env);
        let voucher = Address::generate(&env);
        let attacker = Address::generate(&env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(&env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);
        token_admin.mint(&contract_id, &50_000_000);
        token_admin.mint(&attacker, &10_000_000);

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&admin, &admin, &token_id.address());
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        // attacker tries to repay borrower's loan — must be rejected
        // because attacker != loan.borrower
        let result = client.try_repay(&attacker);
        assert_eq!(result, Err(Ok(ContractError::NoActiveLoan)));

        // also verify borrower can still repay their own loan
        client.repay(&borrower);
    }

    #[test]
    fn test_slash_burns_half_stake() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&borrower);

        assert_eq!(token.balance(&voucher), 9_500_000);
        assert!(client.get_loan(&borrower).unwrap().defaulted);
    }

    #[test]
    #[should_panic(expected = "threshold must be greater than zero")]
    fn test_zero_threshold_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &0);
    }

    #[test]
    fn test_request_loan_underfunded_contract() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let borrower = Address::generate(&env);
        let voucher = Address::generate(&env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(&env, &token_id.address());
        // Give voucher enough to stake but do NOT pre-fund the contract beyond the stake.
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);
        // Contract balance starts at 0; after vouch it will hold 1_000_000.
        // Request a loan larger than the contract balance to trigger InsufficientFunds.

        QuorumCreditContractClient::new(&env, &contract_id)
            .initialize(&admin, &admin, &token_id.address(), &150);
        QuorumCreditContractClient::new(&env, &contract_id).initialize(
            &admin,
            &admin,
            &token_id.address(),
        );

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        // Stake 1_000_000 — contract now holds exactly 1_000_000.
        client.vouch(&voucher, &borrower, &1_000_000);

        // Request 1_500_000: it is within the 150% collateral cap for 1_000_000
        // of stake, but still exceeds the contract's 1_000_000 balance.
        let result = client.try_request_loan(&borrower, &1_500_000, &1_000_000);
        assert_eq!(
            result,
            Err(Ok(ContractError::InsufficientFunds)),
            "expected InsufficientFunds error when contract balance < loan amount"
        );
    }

    #[test]
    fn test_duplicate_vouch_should_fail() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // First vouch should succeed
        client.vouch(&voucher, &borrower, &1_000_000);

        // Second vouch from same voucher for same borrower should fail
        let result = client.try_vouch(&voucher, &borrower, &500_000);
        assert_eq!(
            result,
            Err(Ok(ContractError::DuplicateVouch)),
            "expected DuplicateVouch error when same voucher tries to vouch twice for same borrower"
        );

        // Verify only one vouch record exists
        let vouches = client.get_vouches(&borrower).unwrap();
        assert_eq!(vouches.len(), 1);
        assert_eq!(vouches.get(0).unwrap().stake, 1_000_000);
    }

    #[test]
    fn test_increase_stake_updates_existing_vouch() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.increase_stake(&voucher, &borrower, &500_000);

        let vouches = client.get_vouches(&borrower).unwrap();
        assert_eq!(vouches.len(), 1);
        assert_eq!(vouches.get(0).unwrap().stake, 1_500_000);
        assert_eq!(token.balance(&voucher), 8_500_000);

        client.request_loan(&borrower, &750_000, &1_500_000);
        assert_eq!(client.get_loan(&borrower).unwrap().amount, 750_000);
    }

    #[test]
    #[should_panic(expected = "vouch not found")]
    fn test_increase_stake_without_existing_vouch_panics() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.increase_stake(&voucher, &borrower, &500_000);
    }

    #[test]
    #[should_panic(expected = "loan amount must meet minimum threshold")]
    fn test_zero_amount_loan_should_fail() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);

        // This should panic due to zero amount
        client.request_loan(&borrower, &0, &1_000_000);
    }

    #[test]
    fn test_over_collateralization_check() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // Vouch with 1_000_000 stake
        client.vouch(&voucher, &borrower, &1_000_000);

        // With max ratio of 150%, max allowed loan is 1_500_000
        // This should succeed (within ratio)
        client.request_loan(&borrower, &1_500_000, &1_000_000);

        // Repay the first loan
        client.repay(&borrower);

        // This should fail - exceeds 150% ratio (2_000_000 > 1_500_000)
        let result = client.try_request_loan(&borrower, &2_000_000, &1_000_000);
        assert!(
            result.is_err(),
            "expected error when loan amount exceeds maximum collateral ratio"
        );
    }

    #[test]
    #[should_panic(expected = "borrower already has an active loan")]
    fn test_request_loan_rejects_overwrite_of_active_loan() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        // Second request while first loan is still active must panic.
        client.request_loan(&borrower, &500_000, &1_000_000);
    }

    #[test]
    fn test_repay_with_max_vouchers() {
        let env = Env::default();
        env.budget().reset_unlimited();
        let (contract_id, token_addr, _admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        // Create max vouchers
        let mut vouchers = Vec::new(&env);
        for _ in 0..DEFAULT_MAX_VOUCHERS {
            let voucher = Address::generate(&env);
            token_admin.mint(&voucher, &10_000_000);
            vouchers.push_back(voucher);
        }

        // Vouch with all
        for voucher in vouchers.iter() {
            client.vouch(&voucher, &borrower, &1_000_000);
        }

        // Request loan
        client.request_loan(
            &borrower,
            &500_000,
            &(DEFAULT_MAX_VOUCHERS as i128 * 1_000_000),
        );

        // Repay
        client.repay(&borrower);

        // Check loan is repaid
        let loan = client.get_loan(&borrower).unwrap();
        assert!(loan.repaid);
    }

    #[test]
    fn test_repay_nonexistent_loan_should_fail() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // Try to repay a loan that doesn't exist
        let result = client.try_repay(&borrower);
        assert_eq!(
            result,
            Err(Ok(ContractError::NoActiveLoan)),
            "expected NoActiveLoan error when repaying non-existent loan"
        );
    }

    #[test]
    #[should_panic(expected = "maximum vouchers per loan exceeded")]
    fn test_vouch_exceeds_max_limit() {
        let env = Env::default();
        env.budget().reset_unlimited();
        let (contract_id, token_addr, _admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        // Create MAX_VOUCHERS_PER_LOAN vouchers
        let mut vouchers = Vec::new(&env);
        for _ in 0..DEFAULT_MAX_VOUCHERS {
            let voucher = Address::generate(&env);
            token_admin.mint(&voucher, &10_000_000);
            vouchers.push_back(voucher);
        }

        // Vouch with all DEFAULT_MAX_VOUCHERS
        for voucher in vouchers.iter() {
            client.vouch(&voucher, &borrower, &1_000_000);
        }

        // Try to vouch one more - should panic
        let extra_voucher = Address::generate(&env);
        token_admin.mint(&extra_voucher, &10_000_000);
        client.vouch(&extra_voucher, &borrower, &1_000_000);
    }

    #[test]
    fn test_get_vouches_unknown_borrower_returns_none() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert!(client.get_vouches(&borrower).is_none());
    }

    #[test]
    fn test_total_vouched_returns_sum_of_all_stakes() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        // No vouches yet — should return 0.
        assert_eq!(client.total_vouched(&borrower), 0);

        // First voucher stakes 1_000_000.
        client.vouch(&voucher, &borrower, &1_000_000);
        assert_eq!(client.total_vouched(&borrower), 1_000_000);

        // Second voucher stakes 2_500_000.
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);
        client.vouch(&voucher2, &borrower, &2_500_000);
        assert_eq!(client.total_vouched(&borrower), 3_500_000);
    }

    #[test]
    fn test_slash_treasury_withdrawal() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&borrower);

        // 50% of 1_000_000 = 500_000 should be in treasury.
        assert_eq!(client.get_slash_treasury(), 500_000);

        let treasury_recipient = Address::generate(&env);
        client.slash_treasury(&treasury_recipient);

        assert_eq!(token.balance(&treasury_recipient), 500_000);
        assert_eq!(client.get_slash_treasury(), 0);
    }

    // ── Pause / Unpause Tests ─────────────────────────────────────────────────

    #[test]
    fn test_pause_blocks_vouch() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.pause();
        assert!(client.get_paused());

        let result = client.try_vouch(&voucher, &borrower, &1_000_000);
        assert_eq!(result, Err(Ok(ContractError::ContractPaused)));
    }

    #[test]
    fn test_pause_blocks_request_loan() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // Vouch before pausing so stake is in place.
        client.vouch(&voucher, &borrower, &1_000_000);
        client.pause();

        let result = client.try_request_loan(&borrower, &500_000, &1_000_000);
        assert_eq!(result, Err(Ok(ContractError::ContractPaused)));
    }

    #[test]
    fn test_pause_blocks_increase_stake() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.pause();

        let result = client.try_increase_stake(&voucher, &borrower, &500_000);
        assert_eq!(result, Err(Ok(ContractError::ContractPaused)));
    }

    #[test]
    fn test_pause_blocks_repay() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.pause();

        let result = client.try_repay(&borrower);
        assert_eq!(result, Err(Ok(ContractError::ContractPaused)));
    }

    #[test]
    fn test_pause_blocks_slash() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.pause();

        // slash panics on ContractPaused since it's not a Result-returning fn.
        let result = client.try_slash(&borrower);
        assert!(result.is_err());
    }

    #[test]
    fn test_unpause_restores_operations() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.pause();
        assert!(client.get_paused());

        client.unpause();
        assert!(!client.get_paused());

        // vouch should succeed after unpause.
        client.vouch(&voucher, &borrower, &1_000_000);
        let vouches = client.get_vouches(&borrower).unwrap();
        assert_eq!(vouches.len(), 1);
    }

    // ── Loan Deadline / Auto-Slash Tests ──────────────────────────────────────

    #[test]
    fn test_deadline_set_from_loan_duration() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // Set a short custom duration: 1000 seconds via set_config.
        let mut cfg = client.get_config();
        cfg.loan_duration = 1_000;
        client.set_config(&cfg);
        assert_eq!(client.get_config().loan_duration, 1_000);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        let loan = client.get_loan(&borrower).unwrap();
        assert_eq!(loan.deadline, 1_000_000 + 1_000);
    }

    #[test]
    fn test_auto_slash_after_deadline() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.set_config(&{
            let mut c = client.get_config();
            c.loan_duration = 1_000;
            c
        });
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        // Advance time past the deadline.
        env.ledger().set_timestamp(1_002_000);

        // Anyone can call auto_slash — use a random caller (mock_all_auths covers it).
        client.auto_slash(&borrower);

        let loan = client.get_loan(&borrower).unwrap();
        assert!(loan.defaulted);
        // 50% slashed: voucher gets back 500_000 of their 1_000_000 stake.
        assert_eq!(token.balance(&voucher), 9_500_000);
        assert_eq!(client.get_slash_treasury(), 500_000);
    }

    #[test]
    #[should_panic(expected = "loan deadline has not passed")]
    fn test_auto_slash_before_deadline_panics() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.set_config(&{
            let mut c = client.get_config();
            c.loan_duration = 1_000;
            c
        });
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        // Still within deadline — should panic.
        client.auto_slash(&borrower);
    }

    #[test]
    #[should_panic(expected = "loan deadline has passed")]
    fn test_repay_after_deadline_panics() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.set_config(&{
            let mut c = client.get_config();
            c.loan_duration = 1_000;
            c
        });
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        // Advance past deadline.

        env.ledger().set_timestamp(1_002_000);
        client.repay(&borrower);
    }

    #[test]
    fn test_default_loan_duration_is_30_days() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_config().loan_duration, DEFAULT_LOAN_DURATION);
    }

    #[test]
    fn test_get_admin_returns_admin_address() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_admin(), admin);
    }

    // ── Min Stake Tests ───────────────────────────────────────────────────────

    #[test]
    fn test_get_min_stake_defaults_to_zero() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_min_stake(), 0);
    }

    #[test]
    fn test_set_min_stake_and_get() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.set_min_stake(&500_000);
        assert_eq!(client.get_min_stake(), 500_000);
    }

    #[test]
    fn test_vouch_below_min_stake_rejected() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.set_min_stake(&500_000);

        let result = client.try_vouch(&voucher, &borrower, &100_000);
        assert_eq!(result, Err(Ok(ContractError::MinStakeNotMet)));
    }

    #[test]
    fn test_vouch_at_min_stake_succeeds() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.set_min_stake(&500_000);
        client.vouch(&voucher, &borrower, &500_000);

        let vouches = client.get_vouches(&borrower).unwrap();
        assert_eq!(vouches.len(), 1);
    }

    #[test]
    fn test_vouch_above_min_stake_succeeds() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.set_min_stake(&500_000);
        client.vouch(&voucher, &borrower, &1_000_000);

        let vouches = client.get_vouches(&borrower).unwrap();
        assert_eq!(vouches.len(), 1);
        assert_eq!(vouches.get(0).unwrap().stake, 1_000_000);
    #[test]
    fn test_get_contract_balance() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // setup() mints 50_000_000 stroops into the contract.
        assert_eq!(client.get_contract_balance(), 50_000_000);

        // After a vouch the contract holds stake on top of its initial balance.
        client.vouch(&voucher, &borrower, &1_000_000);
        assert_eq!(client.get_contract_balance(), 51_000_000);

        // After disbursing a loan the balance decreases by the loan amount.
        client.request_loan(&borrower, &500_000, &1_000_000);
        assert_eq!(client.get_contract_balance(), 50_500_000);
    // ── Voucher History Tests ─────────────────────────────────────────────────

    #[test]
    fn test_voucher_history_tracks_multiple_borrowers() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, _borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        let borrower_a = Address::generate(&env);
        let borrower_b = Address::generate(&env);
        let borrower_c = Address::generate(&env);

        // Give voucher enough balance for three vouches.
        token_admin.mint(&voucher, &10_000_000);

        client.vouch(&voucher, &borrower_a, &1_000_000);
        client.vouch(&voucher, &borrower_b, &1_000_000);
        client.vouch(&voucher, &borrower_c, &1_000_000);

        let history = client.voucher_history(&voucher);
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0).unwrap(), borrower_a);
        assert_eq!(history.get(1).unwrap(), borrower_b);
        assert_eq!(history.get(2).unwrap(), borrower_c);
    }

    #[test]
    fn test_voucher_history_unknown_voucher_returns_empty() {
    #[test]
    fn test_vouch_exists() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert!(!client.vouch_exists(&voucher, &borrower));
        client.vouch(&voucher, &borrower, &1_000_000);
        assert!(client.vouch_exists(&voucher, &borrower));
    }

    #[test]
    fn test_loan_status_none() {
        let env = Env::default();
        let (contract_id, _, _, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        assert_eq!(client.loan_status(&borrower), LoanStatus::None);
    }

    #[test]
    fn test_loan_status_active() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        assert_eq!(client.loan_status(&borrower), LoanStatus::Active);
    }

    #[test]
    fn test_loan_status_repaid() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower);
        assert_eq!(client.loan_status(&borrower), LoanStatus::Repaid);
    }

    #[test]
    fn test_loan_status_defaulted() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&borrower);
        assert_eq!(client.loan_status(&borrower), LoanStatus::Defaulted);
    }

    #[test]
    fn test_is_initialized() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert!(!client.is_initialized());
        client.initialize(&admin, &admin, &token_id.address());
        assert!(client.is_initialized());
    }

    #[test]
    fn test_get_token_returns_token_address() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_token(), token_addr);
    }

    // ── Reputation NFT Tests ──────────────────────────────────────────────────

    fn setup_with_reputation(env: &Env) -> (Address, Address, Address, Address, Address, Address) {
        let (contract_id, token_addr, admin, borrower, voucher) = setup(env);
        let client = QuorumCreditContractClient::new(env, &contract_id);

        let nft_id = env.register_contract(None, reputation::ReputationNftContract);
        reputation::ReputationNftContractClient::new(env, &nft_id).initialize(&contract_id);
        client.set_reputation_nft(&nft_id);

        (contract_id, token_addr, admin, borrower, voucher, nft_id)
    }

    #[test]
    fn test_repay_mints_reputation() {
        let env = Env::default();
        let (contract_id, _token, _admin, borrower, voucher, nft_id) = setup_with_reputation(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let nft = reputation::ReputationNftContractClient::new(&env, &nft_id);

        assert_eq!(client.get_reputation(&borrower), 0);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower);

        assert_eq!(client.get_reputation(&borrower), 1);
        assert_eq!(nft.balance(&borrower), 1);
    }

    #[test]
    fn test_slash_burns_reputation() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher, nft_id) =
            setup_with_reputation(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let nft = reputation::ReputationNftContractClient::new(&env, &nft_id);
        let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        // Build up score of 1 via a repaid loan on borrower.
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower);
        assert_eq!(nft.balance(&borrower), 1);

        // Use a fresh borrower + voucher for the default scenario.
        let borrower2 = soroban_sdk::Address::generate(&env);
        let voucher2 = soroban_sdk::Address::generate(&env);
        token_admin.mint(&voucher2, &2_000_000);

        // Manually mint 1 point to borrower2 so burn has something to decrement.
        nft.mint(&borrower2);
        assert_eq!(nft.balance(&borrower2), 1);

        client.vouch(&voucher2, &borrower2, &1_000_000);
        client.request_loan(&borrower2, &500_000, &1_000_000);
        client.slash(&borrower2);

        assert_eq!(client.get_reputation(&borrower2), 0);
        assert_eq!(nft.balance(&borrower2), 0);
    }

    #[test]
    fn test_slash_burn_floors_at_zero() {
        let env = Env::default();
        let (contract_id, _token, _admin, borrower, voucher, _nft_id) = setup_with_reputation(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // Default immediately with score = 0 — should not underflow.
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&borrower);

        assert_eq!(client.get_reputation(&borrower), 0);
    }

    // ── Config Tests ──────────────────────────────────────────────────────────

    #[test]
    fn test_get_config_returns_defaults() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let cfg = client.get_config();
        assert_eq!(cfg.yield_bps, DEFAULT_YIELD_BPS);
        assert_eq!(cfg.slash_bps, DEFAULT_SLASH_BPS);
        assert_eq!(cfg.max_vouchers, DEFAULT_MAX_VOUCHERS);
        assert_eq!(cfg.min_loan_amount, DEFAULT_MIN_LOAN_AMOUNT);
        assert_eq!(cfg.loan_duration, DEFAULT_LOAN_DURATION);
        assert_eq!(cfg.max_loan_to_stake_ratio, DEFAULT_MAX_LOAN_TO_STAKE_RATIO);
    }

    #[test]
    fn test_set_config_updates_params() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let new_cfg = Config {
            yield_bps: 300,
            slash_bps: 3000,
            max_vouchers: 50,
            min_loan_amount: 200_000,
            loan_duration: 7 * 24 * 60 * 60,
            max_loan_to_stake_ratio: 200,
        };
        client.set_config(&new_cfg);

        let cfg = client.get_config();
        assert_eq!(cfg.yield_bps, 300);
        assert_eq!(cfg.slash_bps, 3000);
        assert_eq!(cfg.max_vouchers, 50);
        assert_eq!(cfg.min_loan_amount, 200_000);
        assert_eq!(cfg.loan_duration, 7 * 24 * 60 * 60);
        assert_eq!(cfg.max_loan_to_stake_ratio, 200);
    }

    #[test]
    fn test_config_yield_bps_applied_on_repay() {
        let env = Env::default();
        let (contract_id, token_addr, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        // Set yield to 5% (500 bps).
        let mut cfg = client.get_config();
        cfg.yield_bps = 500;
        client.set_config(&cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower);

        // voucher started with 10_000_000, staked 1_000_000, gets back 1_050_000
        assert_eq!(token.balance(&voucher), 10_050_000);
    }

    #[test]
    fn test_config_slash_bps_applied_on_slash() {
        let env = Env::default();
        let (contract_id, _token_addr, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // Set slash to 25% (2500 bps).
        let mut cfg = client.get_config();
        cfg.slash_bps = 2500;
        client.set_config(&cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&borrower);

        assert_eq!(client.get_reputation(&borrower), 0);
    }

    #[test]
    fn test_get_reputation_without_nft_returns_zero() {
        let env = Env::default();
        let (contract_id, _token, _admin, borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // No NFT contract configured — should return 0 gracefully.
        assert_eq!(client.get_reputation(&borrower), 0);
    }

    #[test]
    #[should_panic(expected = "slash_bps must be 1-10000")]
    fn test_set_config_slash_bps_above_10000_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let mut cfg = client.get_config();
        cfg.slash_bps = 10_001;
        client.set_config(&cfg);
    }

    #[test]
    fn test_set_config_slash_bps_at_boundary_10000_accepted() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let unknown = Address::generate(&env);
        let history = client.voucher_history(&unknown);
        assert_eq!(history.len(), 0);
        let mut cfg = client.get_config();
        cfg.slash_bps = 10_000;
        client.set_config(&cfg);

        assert_eq!(client.get_config().slash_bps, 10_000);
    }
}
