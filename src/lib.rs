#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
    Vec,
};

pub mod reputation;
use reputation::ReputationNftExternalClient;

// ── Constants (defaults only) ─────────────────────────────────────────────────

const DEFAULT_YIELD_BPS: i128 = 200;
const DEFAULT_SLASH_BPS: i128 = 5000;
const _: () = assert!(
    DEFAULT_YIELD_BPS > 0 && DEFAULT_YIELD_BPS <= 10_000,
    "DEFAULT_YIELD_BPS must be in range 1..=10_000"
);
const _: () = assert!(
    DEFAULT_SLASH_BPS > 0 && DEFAULT_SLASH_BPS <= 10_000,
    "DEFAULT_SLASH_BPS must be in range 1..=10_000"
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
    PoolLengthMismatch = 6,
    PoolEmpty = 7,
    PoolBorrowerActiveLoan = 8,
    PoolInsufficientFunds = 9,
    MinStakeNotMet = 10,
    LoanExceedsMaxAmount = 11,
    InsufficientVouchers = 12,
    UnauthorizedCaller = 13,
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
    Config,                  // Config struct: all configurable protocol parameters
    Deployer,                // Address that deployed the contract; guards initialize
    SlashTreasury,           // i128 accumulated slashed funds
    Paused,                  // bool: true when contract is paused
    ReputationNft,           // Address of the ReputationNftContract
    MinStake,                // i128 minimum stake amount per vouch
    MaxLoanAmount,           // i128 maximum individual loan size (0 = no cap)
    MinVouchers,             // u32 minimum number of distinct vouchers required (0 = no minimum)
    LoanPool(u64),           // pool_id → LoanPoolRecord
    LoanPoolCounter,         // u64: monotonically increasing pool ID counter
    PendingAdmin,            // Address of the pending admin (two-step transfer)
    RepaymentCount(Address), // borrower → u32 total successful repayments
    ProtocolFeeBps,          // u32: protocol fee in basis points
}

// ── Config ────────────────────────────────────────────────────────────────────

/// All configurable protocol parameters, stored under DataKey::Config.
#[contracttype]
#[derive(Clone)]
pub struct Config {
    /// Admin addresses for multisig governance.
    pub admins: Vec<Address>,
    /// Number of admin signatures required.
    pub admin_threshold: u32,
    /// XLM token contract address.
    pub token: Address,
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

// ── Data Types ────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub struct LoanRecord {
    pub borrower: Address,
    pub amount: i128,        // total loan principal in stroops
    pub amount_repaid: i128, // cumulative repayments received so far
    pub repaid: bool,
    pub defaulted: bool,
    pub created_at: u64,             // ledger timestamp
    pub disbursement_timestamp: u64, // ledger timestamp
    pub deadline: u64,               // repayment deadline (ledger timestamp)
}

#[contracttype]
#[derive(Clone)]
pub struct VouchRecord {
    pub voucher: Address,
    pub stake: i128,          // in stroops
    pub vouch_timestamp: u64, // ledger timestamp when vouch was created; immutable after set
}

/// A record of a loan pool created via create_loan_pool.
#[contracttype]
#[derive(Clone)]
pub struct LoanPoolRecord {
    pub pool_id: u64,
    pub borrowers: Vec<Address>,
    pub amounts: Vec<i128>,
    pub created_at: u64,
    pub total_disbursed: i128,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct QuorumCreditContract;

#[contractimpl]
impl QuorumCreditContract {
    /// One-time initialisation: set admins, XLM token address, and default config.
    ///
    /// `deployer` must be the address that deployed this contract and must
    /// sign this transaction. This prevents front-running attacks.
    pub fn initialize(
        env: Env,
        deployer: Address,
        admins: Vec<Address>,
        admin_threshold: u32,
        token: Address,
    ) {
        deployer.require_auth();

        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, ContractError::AlreadyInitialized);
        }
        assert!(
            !env.storage().instance().has(&DataKey::Config),
            "already initialized"
        );
        Self::validate_admin_config(&admins, admin_threshold);
        assert!(
            DEFAULT_YIELD_BPS > 0 && DEFAULT_YIELD_BPS <= 10_000,
            "yield_bps must be in range 1..=10000"
        );
        assert!(
            DEFAULT_SLASH_BPS > 0 && DEFAULT_SLASH_BPS <= 10_000,
            "slash_bps must be in range 1..=10000"
        );

        env.storage().instance().set(&DataKey::Deployer, &deployer);
        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admins,
                admin_threshold,
                token,
                yield_bps: DEFAULT_YIELD_BPS,
                slash_bps: DEFAULT_SLASH_BPS,
                max_vouchers: DEFAULT_MAX_VOUCHERS,
                min_loan_amount: DEFAULT_MIN_LOAN_AMOUNT,
                loan_duration: DEFAULT_LOAN_DURATION,
                max_loan_to_stake_ratio: DEFAULT_MAX_LOAN_TO_STAKE_RATIO,
            },
        );
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

        // Validate numeric input: stake must be strictly positive.
        Self::require_positive_amount(&env, stake)?;

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

        vouches.push_back(VouchRecord {
            voucher: voucher.clone(),
            stake,
            vouch_timestamp: env.ledger().timestamp(),
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

        // Validate numeric input: additional must be strictly positive.
        Self::require_positive_amount(&env, additional)?;

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

    /// Reduce stake from an existing vouch before any active loan exists.
    pub fn decrease_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        amount: i128,
    ) -> Result<(), ContractError> {
        voucher.require_auth();
        Self::require_not_paused(&env)?;

        assert!(amount > 0, "decrease amount must be greater than zero");
        assert!(
            !Self::has_active_loan(&env, &borrower),
            "loan already active"
        );

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
        assert!(
            amount <= vouch.stake,
            "decrease amount exceeds staked amount"
        );

        vouch.stake -= amount;
        if vouch.stake == 0 {
            vouches.remove(idx);
        } else {
            vouches.set(idx, vouch);
        }

        if vouches.is_empty() {
            env.storage()
                .persistent()
                .remove(&DataKey::Vouches(borrower));
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::Vouches(borrower), &vouches);
        }

        Self::token(&env).transfer(&env.current_contract_address(), &voucher, &amount);

        Ok(())
    }

    /// Disburse a microloan if total vouched stake meets the threshold.
    pub fn request_loan(
        env: Env,
        borrower: Address,
        co_borrowers: Vec<Address>,
        amount: i128,
        threshold: i128,
    ) -> Result<(), ContractError> {
        borrower.require_auth();
        for cb in co_borrowers.iter() {
            cb.require_auth();
        }
        Self::require_not_paused(&env)?;

        let cfg = Self::config(&env);

        assert!(
            amount >= cfg.min_loan_amount,
            "loan amount must meet minimum threshold"
        );
        // Validate threshold is strictly positive.
        Self::require_positive_amount(&env, threshold)?;

        // Enforce max loan amount cap if configured.
        let max_loan: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxLoanAmount)
            .unwrap_or(0);
        if max_loan > 0 && amount > max_loan {
            return Err(ContractError::LoanExceedsMaxAmount);
        }

        // Prevent overwriting an active loan record.
        assert!(
            !Self::has_active_loan(&env, &borrower),
            "borrower already has an active loan"
        );

        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        let total_stake: i128 = vouches.iter().map(|v| v.stake).sum();
        assert!(total_stake >= threshold, "insufficient trust stake");

        // Enforce minimum voucher count if configured.
        let min_vouchers: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MinVouchers)
            .unwrap_or(0);
        if vouches.len() < min_vouchers {
            return Err(ContractError::InsufficientVouchers);
        }

        // Check collateral ratio: amount must not exceed total_stake * ratio / 100
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
                co_borrowers,
                amount,
                amount_repaid: 0,
                repaid: false,
                defaulted: false,
                created_at: now,
                disbursement_timestamp: now,
                deadline,
                slash_timestamp: None,
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

    /// Borrower repays all or part of the loan.
    ///
    /// `payment` is the amount being paid in this call (in stroops). It must be
    /// at least 1 stroop and cannot exceed the outstanding balance. When the
    /// cumulative `amount_repaid` reaches `amount`, the loan is marked fully
    /// repaid and each voucher receives their stake back plus a proportional
    /// share of the yield (proportional to their stake / total_stake).
    pub fn repay(env: Env, borrower: Address, payment: i128) -> Result<(), ContractError> {
        // ── CHECKS ────────────────────────────────────────────────────────────
        borrower.require_auth();
        Self::require_not_paused(&env)?;

        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(borrower.clone()))
            .ok_or(ContractError::NoActiveLoan)?;

        // All co-borrowers must also authorize the repayment.
        for cb in loan.co_borrowers.iter() {
            cb.require_auth();
        }

        if borrower != loan.borrower {
            return Err(ContractError::UnauthorizedCaller);
        // Guard: only an active (non-repaid, non-defaulted) loan may be repaid.
        if loan.defaulted || loan.repaid {
            return Err(ContractError::InvalidStateTransition);
        }

        // Block repayment after deadline — borrower must be auto-slashed instead.
        assert!(!loan.defaulted, "loan already defaulted");
        assert!(!loan.repaid, "loan already repa
        assert!(
            env.ledger().timestamp() <= loan.deadline,
            "loan deadline has passed"
        );

        let outstanding = loan.amount - loan.amount_repaid;
        assert!(
            payment > 0 && payment <= outstanding,
            "invalid payment amount"
        );

        let token = Self::token(&env);

        // Collect this installment from the borrower.
        token.transfer(&borrower, &env.current_contract_address(), &payment);
        loan.amount_repaid += payment;

        if loan.amount_repaid >= loan.amount {
            // Fully repaid — distribute stake + proportional yield to each voucher.
            let cfg = Self::config(&env);
            let vouches: Vec<VouchRecord> = env
                .storage()
                .persistent()
                .get(&DataKey::Vouches(borrower.clone()))
                .unwrap_or(Vec::new(&env));

            let total_stake: i128 = vouches.iter().map(|v| v.stake).sum();

            // Total yield pool = loan.amount * yield_bps / 10_000
            let total_yield = loan.amount * cfg.yield_bps / 10_000;

        // ── EFFECTS (all state mutations before any outbound transfer) ─────────
        if fully_repaid {
            // Pre-check contract balance covers all payouts before committing.
            let total_payout: i128 = vouches.iter().map(|v| {
                let yield_amount = v.stake * cfg.yield_bps / 10_000;
                v.stake + yield_amount
            }).sum();
            let contract_balance = token.balance(&env.current_contract_address());
            assert!(
                contract_balance >= total_payout,
                "insufficient contract balance for yield distribution"
            );

            // Return stake + yield to each voucher.
            for v in vouches.iter() {
                let voucher_yield = if total_stake > 0 {
                    total_yield * v.stake / total_stake
                } else {
                    0
                };
                token.transfer(
                    &env.current_contract_address(),
                    &v.voucher,
                    &(v.stake + voucher_yield),
                );
            }

            loan.repaid = true;

            // Increment successful repayment count for the borrower.
            let count: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::RepaymentCount(borrower.clone()))
                .unwrap_or(0);
            env.storage()
                .persistent()
                .set(&DataKey::RepaymentCount(borrower.clone()), &(count + 1));

            // Mint one reputation point if a reputation NFT contract is configured.
            if let Some(nft_addr) = env
                .storage()
                .instance()
                .get::<DataKey, Address>(&DataKey::ReputationNft)
            {
                ReputationNftContractClient::new(&env, &nft_addr).mint(&borrower);
            }
        }

        // Persist the updated loan record (amount_repaid + repaid flag) before
        // any outbound transfers so the guard is live in storage.
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower.clone()), &loan);

        Ok(())
    }

    /// Admin marks a loan defaulted; slash_bps% of each voucher's stake is slashed.
    pub fn slash(env: Env, admin_signers: Vec<Address>, borrower: Address) {
        Self::require_admin_approval(&env, &admin_signers);

        Self::require_not_paused(&env).expect("contract is paused");

        // ── CHECKS ────────────────────────────────────────────────────────────
        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(borrower.clone()))
            .expect("no active loan");

        // Guard: only an active (non-repaid, non-defaulted) loan may be slashed.
        if loan.repaid || loan.defaulted {
            panic_with_error!(&env, ContractError::InvalidStateTransition);
        }

        let cfg = Self::config(&env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        let mut total_slashed: i128 = 0;
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
            total_slashed += slash_amount;
        }

        // Burn one reputation point if a reputation NFT contract is configured.
        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftExternalClient::new(&env, &nft_addr).burn(&borrower);
        }

        // Clear vouches after slashing to prevent state pollution.
        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower.clone()));

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("slashed")),
            (borrower, loan.amount, total_slashed),
        );
    }

    /// Allows vouchers to claim back their stake if loan has expired without repayment or slash.
    /// Requires the borrower's authorisation — they acknowledge the loan has lapsed.
    pub fn claim_expired_loan(env: Env, borrower: Address) {
        borrower.require_auth();

        let loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(borrower.clone()))
            .expect("no active loan");

        // Guard: only an active (non-repaid, non-defaulted) loan may be claimed.
        if loan.repaid || loan.defaulted {
            panic_with_error!(&env, ContractError::InvalidStateTransition);
        }

        let now = env.ledger().timestamp();
        assert!(now >= loan.deadline, "loan has not expired yet");

        let token = Self::token(&env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        for v in vouches.iter() {
            token.transfer(&env.current_contract_address(), &v.voucher, &v.stake);
        }

        let mut loan = loan;
        loan.defaulted = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower.clone()), &loan);

        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower));
    }

    /// Admin withdraws accumulated slashed funds to a recipient address.

    pub fn slash_treasury(env: Env, recipient: Address) {
        Self::require_admin(&env);

    pub fn slash_treasury(env: Env, admin_signers: Vec<Address>, recipient: Address) {
        Self::require_admin_approval(&env, &admin_signers);


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

        assert!(
            env.storage()
                .persistent()
                .get::<DataKey, LoanRecord>(&DataKey::Loan(borrower.clone()))
                .is_none(),
            "loan already active"
        );

        let mut vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .expect("vouch not found");

        let idx = vouches
            .iter()
            .position(|v| v.voucher == voucher)
            .expect("vouch not found") as u32;

        let stake = vouches.get(idx).unwrap().stake;
        vouches.remove(idx);

        if vouches.is_empty() {
            env.storage()
                .persistent()
                .remove(&DataKey::Vouches(borrower));
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::Vouches(borrower), &vouches);
        }

        Self::token(&env).transfer(&env.current_contract_address(), &voucher, &stake);
    }

    // ── Loan Deadline ─────────────────────────────────────────────────────────

    /// Callable by anyone after the loan deadline has passed.
    /// Applies the standard slash penalty.
    pub fn auto_slash(env: Env, borrower: Address) {
        // ── CHECKS ────────────────────────────────────────────────────────────
        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(borrower.clone()))
            .expect("no active loan");

        // Guard: only an active (non-repaid, non-defaulted) loan may be auto-slashed.
        if loan.repaid || loan.defaulted {
            panic_with_error!(&env, ContractError::InvalidStateTransition);
        }
        assert!(
            env.ledger().timestamp() > loan.deadline,
            "loan deadline has not passed"
        );

        let cfg = Self::config(&env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        // ── EFFECTS ───────────────────────────────────────────────────────────
        loan.defaulted = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower.clone()), &loan);

        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower.clone()));

        let mut total_slash: i128 = 0;
        for v in vouches.iter() {
            total_slash += v.stake * cfg.slash_bps / 10_000;
        }
        let treasury: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SlashTreasury)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::SlashTreasury, &(treasury + total_slash));

        // ── INTERACTIONS ──────────────────────────────────────────────────────
        let token = Self::token(&env);
        for v in vouches.iter() {
            let slash_amount = v.stake * cfg.slash_bps / 10_000;
            let returned = v.stake - slash_amount;
            if returned > 0 {
                token.transfer(&env.current_contract_address(), &v.voucher, &returned);
            }
        }

        loan.defaulted = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower.clone()), &loan);

        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftExternalClient::new(&env, &nft_addr).burn(&borrower);
        }
    }

    // ── Admin Setters ─────────────────────────────────────────────────────────

    /// Admin sets the minimum stake amount required per vouch (in stroops).
    pub fn set_min_stake(env: Env, admin_signers: Vec<Address>, amount: i128) {
        Self::require_admin_approval(&env, &admin_signers);
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

    /// Admin sets the maximum individual loan amount (in stroops).
    pub fn set_max_loan_amount(env: Env, admin_signers: Vec<Address>, amount: i128) {
        Self::require_admin_approval(&env, &admin_signers);
        assert!(amount >= 0, "max loan amount cannot be negative");
        env.storage()
            .instance()
            .set(&DataKey::MaxLoanAmount, &amount);
    }

    /// Returns the current maximum loan amount (0 means no cap).
    pub fn get_max_loan_amount(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::MaxLoanAmount)
            .unwrap_or(0)
    }

    /// Admin sets the minimum number of distinct vouchers required.
    pub fn set_min_vouchers(env: Env, admin_signers: Vec<Address>, count: u32) {
        Self::require_admin_approval(&env, &admin_signers);
        env.storage().instance().set(&DataKey::MinVouchers, &count);
    }

    /// Returns the current minimum voucher count (0 means no minimum).
    pub fn get_min_vouchers(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MinVouchers)
            .unwrap_or(0)
    }

    /// Admin updates configurable protocol parameters.
    pub fn set_config(env: Env, admin_signers: Vec<Address>, config: Config) {
        Self::require_admin_approval(&env, &admin_signers);
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
        Self::validate_admin_config(&config.admins, config.admin_threshold);
        env.storage().instance().set(&DataKey::Config, &config);
    }

    /// Returns the current protocol config.
    pub fn get_config(env: Env) -> Config {
        Self::config(&env)
    }

    /// Admin sets the reputation NFT contract address.
    pub fn set_reputation_nft(env: Env, admin_signers: Vec<Address>, nft_contract: Address) {
        Self::require_admin_approval(&env, &admin_signers);
        env.storage()
            .instance()
            .set(&DataKey::ReputationNft, &nft_contract);
    }

    // ── Admin: Protocol Fee ───────────────────────────────────────────────────

    /// Admin sets the protocol fee applied to interactions (in basis points).
    pub fn set_protocol_fee(env: Env, admin_signers: Vec<Address>, fee_bps: u32) {
        Self::require_admin_approval(&env, &admin_signers);
        assert!(fee_bps <= 10_000, "fee_bps must not exceed 10000");
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &fee_bps);
    }

    /// Returns the current protocol fee (0 if not set).
    pub fn get_protocol_fee(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0)
    }

    // ── Admin: Upgrade ──────────────────────────────────────────────────────

    /// Admin upgrades the contract WASM.
    pub fn upgrade(env: Env, admin_signers: Vec<Address>, new_wasm_hash: BytesN<32>) {
        Self::require_admin_approval(&env, &admin_signers);
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        env.events()
            .publish((symbol_short!("upgrade"),), new_wasm_hash);
    }

    // ── Admin: Pause / Unpause ────────────────────────────────────────────────

    /// Pause the contract.
    pub fn pause(env: Env, admin_signers: Vec<Address>) {
        Self::require_admin_approval(&env, &admin_signers);

        env.storage().instance().set(&DataKey::Paused, &true);
    }

    /// Unpause the contract.
    pub fn unpause(env: Env, admin_signers: Vec<Address>) {
        Self::require_admin_approval(&env, &admin_signers);

        env.storage().instance().set(&DataKey::Paused, &false);
    }

    // ── Views ─────────────────────────────────────────────────────────────────

    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&DataKey::Config)
    }

    pub fn get_token(env: Env) -> Address {
        Self::config(&env).token
    }

    pub fn get_admins(env: Env) -> Vec<Address> {
        Self::config(&env).admins
    }

    pub fn get_admin_threshold(env: Env) -> u32 {
        Self::config(&env).admin_threshold
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

    /// Read-only eligibility check for frontends.
    pub fn is_eligible(env: Env, borrower: Address, threshold: i128) -> bool {
        if threshold <= 0 {
            return false;
        }

        if let Some(loan) = env
            .storage()
            .persistent()
            .get::<DataKey, LoanRecord>(&DataKey::Loan(borrower.clone()))
        {
            if !loan.repaid && !loan.defaulted {
                return false;
            }
        }

        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower))
            .unwrap_or(Vec::new(&env));

        let total_stake: i128 = vouches.iter().map(|v| v.stake).sum();
        total_stake >= threshold
    }

    /// Returns the contract's current XLM balance in stroops.
    pub fn get_contract_balance(env: Env) -> i128 {
        Self::token(&env).balance(&env.current_contract_address())
    }

    /// Returns all borrower addresses that the given voucher has ever backed.
    pub fn voucher_history(env: Env, voucher: Address) -> Vec<Address> {
        env.storage()
            .persistent()
            .get(&DataKey::VoucherHistory(voucher))
            .unwrap_or(Vec::new(&env))
    }

    /// Returns the reputation score for a borrower.
    pub fn get_reputation(env: Env, borrower: Address) -> u32 {
        let nft_addr: Address = match env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            Some(a) => a,
            None => return 0,
        };
        ReputationNftExternalClient::new(&env, &nft_addr).balance(&borrower)
    }

    /// Returns the total staked amount across all vouchers for a given borrower.
    pub fn total_vouched(env: Env, borrower: Address) -> i128 {
        env.storage()
            .persistent()
            .get::<DataKey, Vec<VouchRecord>>(&DataKey::Vouches(borrower))
            .unwrap_or(Vec::new(&env))
            .iter()
            .map(|v| v.stake)
            .sum()
    }

    /// Returns the total number of successful repayments for a borrower.
    pub fn repayment_count(env: Env, borrower: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::RepaymentCount(borrower))
            .unwrap_or(0)
    }

    // ── Loan Pool ─────────────────────────────────────────────────────────────

    /// Admin function: atomically disburse a batch of small loans to multiple borrowers.
    pub fn create_loan_pool(
        env: Env,
        admin_signers: Vec<Address>,
        borrowers: Vec<Address>,
        amounts: Vec<i128>,
    ) -> Result<u64, ContractError> {
        Self::require_admin_approval(&env, &admin_signers);

        if borrowers.len() != amounts.len() {
            return Err(ContractError::PoolLengthMismatch);
        }
        if borrowers.is_empty() {
            return Err(ContractError::PoolEmpty);
        }

        let cfg = Self::config(&env);
        let now = env.ledger().timestamp();
        let deadline = now + cfg.loan_duration;

        let mut total_amount: i128 = 0;
        for i in 0..borrowers.len() {
            let borrower = borrowers.get(i).unwrap();
            let amount = amounts.get(i).unwrap();

            assert!(
                amount >= cfg.min_loan_amount,
                "pool: amount below minimum loan threshold"
            );

            if let Some(existing) = env
                .storage()
                .persistent()
                .get::<DataKey, LoanRecord>(&DataKey::Loan(borrower.clone()))
            {
                if !existing.repaid && !existing.defaulted {
                    return Err(ContractError::PoolBorrowerActiveLoan);
                }
            }

            let total_stake: i128 = env
                .storage()
                .persistent()
                .get::<DataKey, Vec<VouchRecord>>(&DataKey::Vouches(borrower.clone()))
                .unwrap_or(Vec::new(&env))
                .iter()
                .map(|v| v.stake)
                .sum();
            let max_allowed = total_stake * cfg.max_loan_to_stake_ratio as i128 / 100;
            assert!(
                amount <= max_allowed,
                "pool: loan amount exceeds maximum collateral ratio for borrower"
            );

            total_amount += amount;
        }

        let token = Self::token(&env);
        let contract_balance = token.balance(&env.current_contract_address());
        if contract_balance < total_amount {
            return Err(ContractError::PoolInsufficientFunds);
        }

        let pool_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LoanPoolCounter)
            .unwrap_or(0u64)
            .checked_add(1)
            .expect("pool ID overflow");
        env.storage()
            .instance()
            .set(&DataKey::LoanPoolCounter, &pool_id);

        for i in 0..borrowers.len() {
            let borrower = borrowers.get(i).unwrap();
            let amount = amounts.get(i).unwrap();

            env.storage().persistent().set(
                &DataKey::Loan(borrower.clone()),
                &LoanRecord {
                    borrower: borrower.clone(),
                    co_borrowers: Vec::new(&env), // pools currently only use single borrowers
                    amount,
                    amount_repaid: 0,
                    repaid: false,
                    defaulted: false,
                    created_at: now,
                    disbursement_timestamp: now,
                    deadline,
                },
            );

            token.transfer(&env.current_contract_address(), &borrower, &amount);

            env.events().publish(
                (symbol_short!("pool"), symbol_short!("loan")),
                (pool_id, borrower.clone(), amount, deadline),
            );
        }

        env.storage().persistent().set(
            &DataKey::LoanPool(pool_id),
            &LoanPoolRecord {
                pool_id,
                borrowers: borrowers.clone(),
                amounts: amounts.clone(),
                created_at: now,
                total_disbursed: total_amount,
            },
        );

        env.events().publish(
            (symbol_short!("pool"), symbol_short!("created")),
            (pool_id, borrowers.len(), total_amount),
        );

        Ok(pool_id)
    }

    /// Returns the loan pool record for a given pool ID, or None if not found.
    pub fn get_loan_pool(env: Env, pool_id: u64) -> Option<LoanPoolRecord> {
        env.storage().persistent().get(&DataKey::LoanPool(pool_id))
    }

    /// Returns the current pool ID counter.
    pub fn get_loan_pool_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::LoanPoolCounter)
            .unwrap_or(0)
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Loads the stored admin address and calls `require_auth()` on it.
    /// Panics with "not initialized" if the contract has not been initialised.
    ///
    /// # Access-control model
    ///
    /// | Function              | Permitted caller(s)                        |
    /// |-----------------------|--------------------------------------------|
    /// | initialize            | deployer (the account that deployed the contract) |
    /// | vouch                 | voucher (the staking account)              |
    /// | increase_stake        | voucher (must already have a vouch record) |
    /// | withdraw_vouch        | voucher (only before a loan is active)     |
    /// | request_loan          | borrower                                   |
    /// | repay                 | borrower (must match loan.borrower)        |
    /// | claim_expired_loan    | borrower (after loan deadline has passed)  |
    /// | auto_slash            | anyone (permissionless after deadline)     |
    /// | slash                 | admin                                      |
    /// | slash_treasury        | admin                                      |
    /// | set_config            | admin                                      |
    /// | pause / unpause       | admin                                      |
    /// | propose_admin         | admin                                      |
    /// | accept_admin          | pending admin                              |
    /// | set_reputation_nft    | admin                                      |
    /// | get_* / view fns      | anyone (read-only, no auth required)       |
    fn require_admin(env: &Env) -> Address {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();
        admin
    }

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

    /// Returns `Err(InvalidAmount)` if `amount` is not strictly positive (≤ 0).
    /// Use this for all numeric inputs that must be > 0 (stakes, loan amounts, thresholds).
    fn require_positive_amount(_env: &Env, amount: i128) -> Result<(), ContractError> {
        if amount <= 0 {
            return Err(ContractError::InvalidAmount);
        }
        Ok(())
    }

    fn config(env: &Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .expect("not initialized")
    }

    fn has_active_loan(env: &Env, borrower: &Address) -> bool {
        matches!(
            env.storage()
                .persistent()
                .get::<DataKey, LoanRecord>(&DataKey::Loan(borrower.clone())),
            Some(loan) if !loan.repaid && !loan.defaulted
        )
    }

    fn token(env: &Env) -> token::Client<'_> {
        let addr = Self::config(env).token;
        token::Client::new(env, &addr)
    }

    fn require_admin_approval(env: &Env, admin_signers: &Vec<Address>) {
        let config = Self::config(env);
        assert!(
            admin_signers.len() >= config.admin_threshold,
            "insufficient admin approvals"
        );

        let signer_count = admin_signers.len();
        for i in 0..signer_count {
            let signer = admin_signers.get(i).unwrap();

            for j in 0..i {
                let prior_signer = admin_signers.get(j).unwrap();
                assert!(signer != prior_signer, "duplicate admin signer");
            }

            let mut is_admin = false;
            for admin in config.admins.iter() {
                if admin == signer {
                    is_admin = true;
                    break;
                }
            }

            assert!(is_admin, "unauthorized admin signer");
            signer.require_auth();
        }
    }

    fn validate_admin_config(admins: &Vec<Address>, admin_threshold: u32) {
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
            for j in 0..i {
                let prior_admin = admins.get(j).unwrap();
                assert!(admin != prior_admin, "duplicate admin");
            }
        }
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

    fn address_vec(env: &Env, addresses: &[Address]) -> Vec<Address> {
        let mut result = Vec::new(env);
        for address in addresses {
            result.push_back(address.clone());
        }
        result
    }

    fn single_admin_signers(env: &Env, admin: &Address) -> Vec<Address> {
        address_vec(env, core::slice::from_ref(admin))
    }

    fn setup(env: &Env) -> (Address, Address, Address, Address, Address) {
        env.mock_all_auths();

        let admin = Address::generate(env);
        let borrower = Address::generate(env);
        let voucher = Address::generate(env);
        let admins = single_admin_signers(env, &admin);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);
        token_admin.mint(&contract_id, &50_000_000);

        QuorumCreditContractClient::new(env, &contract_id).initialize(
            &admin,
            &admins,
            &1,
            &token_id.address(),
        );

        (contract_id, token_id.address(), admin, borrower, voucher)
    }

    fn setup_multisig(
        env: &Env,
        admin_threshold: u32,
    ) -> (
        Address,
        Address,
        Address,
        Address,
        Address,
        Address,
        Address,
    ) {
        env.mock_all_auths();

        let admin_one = Address::generate(env);
        let admin_two = Address::generate(env);
        let admin_three = Address::generate(env);
        let borrower = Address::generate(env);
        let voucher = Address::generate(env);
        let admins = address_vec(
            env,
            &[admin_one.clone(), admin_two.clone(), admin_three.clone()],
        );

        let token_id = env.register_stellar_asset_contract_v2(admin_one.clone());
        let token_admin = StellarAssetClient::new(env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);
        token_admin.mint(&contract_id, &50_000_000);

        QuorumCreditContractClient::new(env, &contract_id).initialize(
            &admin_one,
            &admins,
            &admin_threshold,
            &token_id.address(),
        );

        (
            contract_id,
            token_id.address(),
            admin_one,
            admin_two,
            admin_three,
            borrower,
            voucher,
        )
    }

    #[test]
    fn test_vouch_and_loan_disbursed() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

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
        let admins = single_admin_signers(&env, &admin);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(&env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);
        token_admin.mint(&contract_id, &50_000_000);

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&admin, &admins, &1, &token_id.address());

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

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
        let admins = single_admin_signers(&env, &admin);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(&env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&admin, &admins, &1, &token_id.address());

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
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower, &500_000);

        assert_eq!(token.balance(&voucher), 10_010_000);
    }

    #[test]
    fn test_repay_mismatched_borrower_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let borrower = Address::generate(&env);
        let voucher = Address::generate(&env);
        let attacker = Address::generate(&env);
        let admins = single_admin_signers(&env, &admin);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(&env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);
        token_admin.mint(&contract_id, &50_000_000);
        token_admin.mint(&attacker, &10_000_000);

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&admin, &admins, &1, &token_id.address());
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        let result = client.try_repay(&attacker, &500_000);
        assert_eq!(result, Err(Ok(ContractError::NoActiveLoan)));

        client.repay(&borrower, &500_000);
    }

    #[test]
    fn test_repay_emits_event() {
        use soroban_sdk::{IntoVal, Val};

        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower);

        let topic_loan: Val = symbol_short!("loan").into_val(&env);
        let topic_repaid: Val = symbol_short!("repaid").into_val(&env);

        let (_, _, data) = env
            .events()
            .all()
            .iter()
            .find(|(_, topics, _)| {
                topics.len() == 2
                    && topics.get_unchecked(0).get_payload() == topic_loan.get_payload()
                    && topics.get_unchecked(1).get_payload() == topic_repaid.get_payload()
            })
            .expect("loan_repaid event not emitted");

        let (event_borrower, event_amount): (Address, i128) = data.into_val(&env);
        assert_eq!(event_borrower, borrower);
        assert_eq!(event_amount, 500_000);
    }

    #[test]
    fn test_slash_burns_half_stake() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.slash(&borrower);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower);

        assert_eq!(token.balance(&voucher), 9_500_000);
        assert!(client.get_loan(&borrower).unwrap().defaulted);
    }

    #[test]
    fn test_slash_emits_event() {
        use soroban_sdk::{IntoVal, Val};

        let env = Env::default();
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower);

        let topic_loan: Val = symbol_short!("loan").into_val(&env);
        let topic_slashed: Val = symbol_short!("slashed").into_val(&env);

        let (_, _, data) = env
            .events()
            .all()
            .iter()
            .find(|(_, topics, _)| {
                topics.len() == 2
                    && topics.get_unchecked(0).get_payload() == topic_loan.get_payload()
                    && topics.get_unchecked(1).get_payload() == topic_slashed.get_payload()
            })
            .expect("loan_slashed event not emitted");

        let (event_borrower, event_loan_amount, event_slashed): (Address, i128, i128) =
            data.into_val(&env);
        assert_eq!(event_borrower, borrower);
        assert_eq!(event_loan_amount, 500_000);
        assert_eq!(event_slashed, 500_000); // 50% of 1_000_000 stake
    }

    #[test]
    #[should_panic(expected = "threshold must be greater than zero")]
    fn test_zero_threshold_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &0);
    }

    #[test]
    fn test_request_loan_underfunded_contract() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let borrower = Address::generate(&env);
        let voucher = Address::generate(&env);
        let admins = single_admin_signers(&env, &admin);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(&env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);

        QuorumCreditContractClient::new(&env, &contract_id).initialize(
            &admin,
            &admins,
            &1,
            &token_id.address(),
        );

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &1_000_000);

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

        client.vouch(&voucher, &borrower, &1_000_000);

        let result = client.try_vouch(&voucher, &borrower, &500_000);
        assert_eq!(result, Err(Ok(ContractError::DuplicateVouch)));

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

        client.request_loan(&borrower, &Vec::new(&env), &750_000, &1_500_000);
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
    fn test_decrease_stake_updates_existing_vouch_and_returns_tokens() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.decrease_stake(&voucher, &borrower, &400_000);

        let vouches = client.get_vouches(&borrower).unwrap();
        assert_eq!(vouches.len(), 1);
        assert_eq!(vouches.get(0).unwrap().stake, 600_000);
        assert_eq!(token.balance(&voucher), 9_400_000);
        assert_eq!(client.total_vouched(&borrower), 600_000);
    }

    #[test]
    fn test_decrease_stake_to_zero_removes_vouch() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.decrease_stake(&voucher, &borrower, &1_000_000);

        assert!(client.get_vouches(&borrower).is_none());
        assert_eq!(client.total_vouched(&borrower), 0);
        assert_eq!(token.balance(&voucher), 10_000_000);
    }

    #[test]
    #[should_panic(expected = "loan already active")]
    fn test_decrease_stake_rejects_active_loan() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        client.decrease_stake(&voucher, &borrower, &100_000);
    }

    #[test]
    #[should_panic(expected = "loan amount must meet minimum threshold")]
    fn test_zero_amount_loan_should_fail() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &0, &1_000_000);
    }

    #[test]
    fn test_over_collateralization_check() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &1_500_000, &1_000_000);
        client.repay(&borrower, &1_500_000);

        let result = client.try_request_loan(&borrower, &2_000_000, &1_000_000);
        assert!(result.is_err());
    }

    #[test]
    #[should_panic(expected = "borrower already has an active loan")]
    fn test_request_loan_rejects_overwrite_of_active_loan() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
    }

    #[test]
    fn test_repay_with_max_vouchers() {
        let env = Env::default();
        env.budget().reset_unlimited();
        let (contract_id, token_addr, _admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        let mut vouchers = Vec::new(&env);
        for _ in 0..DEFAULT_MAX_VOUCHERS {
            let voucher = Address::generate(&env);
            token_admin.mint(&voucher, &10_000_000);
            vouchers.push_back(voucher);
        }

        for voucher in vouchers.iter() {
            client.vouch(&voucher, &borrower, &1_000_000);
        }

        client.request_loan(
            &borrower,
            &Vec::new(&env),
            &500_000,
            &(DEFAULT_MAX_VOUCHERS as i128 * 1_000_000),
        );

        client.repay(&borrower, &500_000);

        let loan = client.get_loan(&borrower).unwrap();
        assert!(loan.repaid);
    }

    #[test]
    fn test_repay_nonexistent_loan_should_fail() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let result = client.try_repay(&borrower, &100_000);
        assert_eq!(result, Err(Ok(ContractError::NoActiveLoan)));
    }

    #[test]
    #[should_panic(expected = "maximum vouchers per loan exceeded")]
    fn test_vouch_exceeds_max_limit() {
        let env = Env::default();
        env.budget().reset_unlimited();
        let (contract_id, token_addr, _admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        let mut vouchers = Vec::new(&env);
        for _ in 0..DEFAULT_MAX_VOUCHERS {
            let voucher = Address::generate(&env);
            token_admin.mint(&voucher, &10_000_000);
            vouchers.push_back(voucher);
        }

        for voucher in vouchers.iter() {
            client.vouch(&voucher, &borrower, &1_000_000);
        }

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

        assert_eq!(client.total_vouched(&borrower), 0);

        client.vouch(&voucher, &borrower, &1_000_000);
        assert_eq!(client.total_vouched(&borrower), 1_000_000);

        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);
        client.vouch(&voucher2, &borrower, &2_500_000);
        assert_eq!(client.total_vouched(&borrower), 3_500_000);
    }

    #[test]
    fn test_slash_treasury_withdrawal() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.slash(&borrower);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower);

        assert_eq!(client.get_slash_treasury(), 500_000);

        let treasury_recipient = Address::generate(&env);
        client.slash_treasury(&admin_signers, &treasury_recipient);

        assert_eq!(token.balance(&treasury_recipient), 500_000);
        assert_eq!(client.get_slash_treasury(), 0);
    }

    // ── Pause / Unpause Tests ─────────────────────────────────────────────────

    #[test]
    fn test_pause_blocks_vouch() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.pause(&admin_signers);
        assert!(client.get_paused());

        let result = client.try_vouch(&voucher, &borrower, &1_000_000);
        assert_eq!(result, Err(Ok(ContractError::ContractPaused)));
    }

    #[test]
    fn test_pause_blocks_request_loan() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.pause(&admin_signers);

        let result = client.try_request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(result, Err(Ok(ContractError::ContractPaused)));
    }

    #[test]
    fn test_pause_blocks_increase_stake() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.pause(&admin_signers);

        let result = client.try_increase_stake(&voucher, &borrower, &500_000);
        assert_eq!(result, Err(Ok(ContractError::ContractPaused)));
    }

    #[test]
    fn test_pause_blocks_decrease_stake() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.pause();

        let result = client.try_decrease_stake(&voucher, &borrower, &500_000);
        assert_eq!(result, Err(Ok(ContractError::ContractPaused)));
    }

    #[test]
    fn test_pause_blocks_repay() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.pause();
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.pause(&admin_signers);

        let result = client.try_repay(&borrower, &500_000);
        assert_eq!(result, Err(Ok(ContractError::ContractPaused)));
    }

    #[test]
    fn test_pause_blocks_slash() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.pause();
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.pause(&admin_signers);

        let result = client.try_slash(&admin_signers, &borrower);
        assert!(result.is_err());
    }

    #[test]
    fn test_unpause_restores_operations() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.pause(&admin_signers);
        assert!(client.get_paused());

        client.unpause(&admin_signers);
        assert!(!client.get_paused());

        client.vouch(&voucher, &borrower, &1_000_000);
        let vouches = client.get_vouches(&borrower).unwrap();
        assert_eq!(vouches.len(), 1);
    }

    // ── Loan Deadline / Auto-Slash Tests ──────────────────────────────────────

    #[test]
    fn test_deadline_set_from_loan_duration() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.loan_duration = 1_000;
        client.set_config(&admin_signers, &cfg);
        assert_eq!(client.get_config().loan_duration, 1_000);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        let loan = client.get_loan(&borrower).unwrap();
        assert_eq!(loan.deadline, 1_000_000 + 1_000);
    }

    #[test]
    fn test_auto_slash_after_deadline() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.loan_duration = 1_000;
        client.set_config(&admin_signers, &cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        env.ledger().set_timestamp(1_002_000);

        client.auto_slash(&borrower);

        let loan = client.get_loan(&borrower).unwrap();
        assert!(loan.defaulted);
        assert_eq!(token.balance(&voucher), 9_500_000);
        assert_eq!(client.get_slash_treasury(), 500_000);
    }

    #[test]
    #[should_panic(expected = "loan deadline has not passed")]
    fn test_auto_slash_before_deadline_panics() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.loan_duration = 1_000;
        client.set_config(&admin_signers, &cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        client.auto_slash(&borrower);
    }

    #[test]
    #[should_panic(expected = "loan deadline has passed")]
    fn test_repay_after_deadline_panics() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.loan_duration = 1_000;
        client.set_config(&admin_signers, &cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        env.ledger().set_timestamp(1_002_000);
        client.repay(&borrower, &500_000);
    }

    #[test]
    fn test_default_loan_duration_is_30_days() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_config().loan_duration, DEFAULT_LOAN_DURATION);
    }

    #[test]
    fn test_get_admin_returns_admin_address() {
        let env = Env::default();
        let (contract_id, _, admin, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let admins = client.get_admins();
        assert_eq!(admins.len(), 1);
        assert_eq!(admins.get(0).unwrap(), admin);
        assert_eq!(client.get_admin_threshold(), 1);
    }

    #[test]
    fn test_loan_records_disbursement_timestamp() {
        let env = Env::default();
        env.ledger().set_timestamp(1_234_567);
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        let loan = client.get_loan(&borrower).unwrap();
        assert_eq!(loan.disbursement_timestamp, 1_234_567);
        assert_eq!(loan.created_at, 1_234_567);
        assert!(loan.deadline > 1_234_567);
    }

    // ── is_eligible Tests ─────────────────────────────────────────────────────

    #[test]
    fn test_is_eligible_returns_true_when_stake_meets_threshold() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);

        assert!(client.is_eligible(&borrower, &1_000_000));
    }

    #[test]
    fn test_is_eligible_returns_false_when_stake_below_threshold() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &500_000);

        assert!(!client.is_eligible(&borrower, &1_000_000));
    }

    #[test]
    fn test_is_eligible_returns_false_for_zero_threshold() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);

        assert!(!client.is_eligible(&borrower, &0));
    }

    #[test]
    fn test_is_eligible_returns_false_when_loan_already_active() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        assert!(!client.is_eligible(&borrower, &1_000_000));
    }

    #[test]
    fn test_is_eligible_returns_true_after_loan_repaid() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower, &500_000);

        assert!(client.is_eligible(&borrower, &1_000_000));
    }

    #[test]
    fn test_is_eligible_returns_false_with_no_vouches() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert!(!client.is_eligible(&borrower, &1_000_000));
    }

    #[test]
    fn test_is_eligible_aggregates_multiple_vouchers() {
        let env = Env::default();
        env.mock_all_auths();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);

        client.vouch(&voucher, &borrower, &600_000);
        client.vouch(&voucher2, &borrower, &600_000);

        assert!(client.is_eligible(&borrower, &1_000_000));
        assert!(client.is_eligible(&borrower, &1_200_000));
        assert!(!client.is_eligible(&borrower, &1_200_001));
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
        let (contract_id, _, admin, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.set_min_stake(&admin_signers, &500_000);
        assert_eq!(client.get_min_stake(), 500_000);
    }

    #[test]
    fn test_vouch_below_min_stake_rejected() {
        let env = Env::default();
        let (contract_id, _, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.set_min_stake(&admin_signers, &500_000);

        let result = client.try_vouch(&voucher, &borrower, &100_000);
        assert_eq!(result, Err(Ok(ContractError::MinStakeNotMet)));
    }

    #[test]
    fn test_vouch_at_min_stake_succeeds() {
        let env = Env::default();
        let (contract_id, _, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.set_min_stake(&admin_signers, &500_000);
        client.vouch(&voucher, &borrower, &500_000);

        let vouches = client.get_vouches(&borrower).unwrap();
        assert_eq!(vouches.len(), 1);
    }

    #[test]
    fn test_vouch_above_min_stake_succeeds() {
        let env = Env::default();
        let (contract_id, _, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.set_min_stake(&admin_signers, &500_000);
        client.vouch(&voucher, &borrower, &1_000_000);

        let vouches = client.get_vouches(&borrower).unwrap();
        assert_eq!(vouches.len(), 1);
        assert_eq!(vouches.get(0).unwrap().stake, 1_000_000);
    }

    // ── Max Loan Amount Tests ─────────────────────────────────────────────────

    #[test]
    fn test_get_max_loan_amount_defaults_to_zero() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        assert_eq!(
            QuorumCreditContractClient::new(&env, &contract_id).get_max_loan_amount(),
            0
        );
    }

    #[test]
    fn test_set_max_loan_amount_and_get() {
        let env = Env::default();
        let (contract_id, _, admin, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);
        client.set_max_loan_amount(&admin_signers, &5_000_000);
        assert_eq!(client.get_max_loan_amount(), 5_000_000);
    }

    #[test]
    fn test_loan_exceeds_max_amount_rejected() {
        let env = Env::default();
        let (contract_id, _, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);
        client.set_max_loan_amount(&admin_signers, &500_000);
        client.vouch(&voucher, &borrower, &5_000_000);
        let result = client.try_request_loan(&borrower, &600_000, &1_000_000);
        assert_eq!(result, Err(Ok(ContractError::LoanExceedsMaxAmount)));
    }

    #[test]
    fn test_loan_at_max_amount_succeeds() {
        let env = Env::default();
        let (contract_id, _, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);
        client.set_max_loan_amount(&admin_signers, &500_000);
        client.vouch(&voucher, &borrower, &5_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        assert_eq!(client.get_loan(&borrower).unwrap().amount, 500_000);
    }

    #[test]
    fn test_no_max_loan_cap_when_zero() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &5_000_000);
        client.request_loan(&borrower, &1_000_000, &1_000_000);
        assert_eq!(client.get_loan(&borrower).unwrap().amount, 1_000_000);
    }

    // ── Min Vouchers Tests ────────────────────────────────────────────────────

    #[test]
    fn test_get_min_vouchers_defaults_to_zero() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        assert_eq!(
            QuorumCreditContractClient::new(&env, &contract_id).get_min_vouchers(),
            0
        );
    }

    #[test]
    fn test_set_min_vouchers_and_get() {
        let env = Env::default();
        let (contract_id, _, admin, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);
        client.set_min_vouchers(&admin_signers, &3);
        assert_eq!(client.get_min_vouchers(), 3);
    }

    #[test]
    fn test_loan_rejected_when_voucher_count_below_minimum() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        client.set_min_vouchers(&admin_signers, &3);
        client.vouch(&voucher, &borrower, &5_000_000);

        let result = client.try_request_loan(&borrower, &500_000, &1_000_000);
        assert_eq!(result, Err(Ok(ContractError::InsufficientVouchers)));

        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);
        client.vouch(&voucher2, &borrower, &1_000_000);
        let result = client.try_request_loan(&borrower, &500_000, &1_000_000);
        assert_eq!(result, Err(Ok(ContractError::InsufficientVouchers)));
    }

    #[test]
    fn test_loan_succeeds_when_voucher_count_meets_minimum() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        client.set_min_vouchers(&admin_signers, &2);

        client.vouch(&voucher, &borrower, &1_000_000);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);
        client.vouch(&voucher2, &borrower, &1_000_000);

        client.request_loan(&borrower, &500_000, &1_000_000);
        assert_eq!(client.get_loan(&borrower).unwrap().amount, 500_000);
    }

    #[test]
    fn test_no_min_vouchers_when_zero() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        assert_eq!(client.get_loan(&borrower).unwrap().amount, 500_000);
    }

    // ── Partial Repayment Tests ───────────────────────────────────────────────

    #[test]
    fn test_partial_repay_updates_amount_repaid() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &600_000, &1_000_000);

        client.repay(&borrower, &200_000);

        let loan = client.get_loan(&borrower).unwrap();
        assert_eq!(loan.amount_repaid, 200_000);
        assert!(!loan.repaid);
    }

    #[test]
    fn test_full_repay_via_installments_marks_repaid_and_pays_yield() {
        let env = Env::default();
        let (contract_id, token_addr, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &600_000, &1_000_000);

        client.repay(&borrower, &400_000);
        assert!(!client.get_loan(&borrower).unwrap().repaid);

        client.repay(&borrower, &200_000);

        let loan = client.get_loan(&borrower).unwrap();
        assert!(loan.repaid);
        assert_eq!(loan.amount_repaid, 600_000);

        assert_eq!(token.balance(&voucher), 10_012_000);
    }

    #[test]
    fn test_single_full_repay_still_works() {
        let env = Env::default();
        let (contract_id, token_addr, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower, &500_000);

        assert!(client.get_loan(&borrower).unwrap().repaid);
        assert_eq!(token.balance(&voucher), 10_010_000);
    }

    #[test]
    #[should_panic(expected = "invalid payment amount")]
    fn test_repay_zero_amount_panics() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower, &0);
    }

    #[test]
    #[should_panic(expected = "invalid payment amount")]
    fn test_repay_exceeds_outstanding_panics() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower, &600_000);
    }

    // ── Contract Balance Tests ────────────────────────────────────────────────

    #[test]
    fn test_get_contract_balance() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_contract_balance(), 50_000_000);

        client.vouch(&voucher, &borrower, &1_000_000);
        assert_eq!(client.get_contract_balance(), 51_000_000);

        client.request_loan(&borrower, &500_000, &1_000_000);
        assert_eq!(client.get_contract_balance(), 50_500_000);
    }

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
        let env = Env::default();
        let (contract_id, _token_addr, _admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let unknown = Address::generate(&env);
        let history = client.voucher_history(&unknown);
        assert_eq!(history.len(), 0);
    }

    // ── Vouch Exists Tests ────────────────────────────────────────────────────

    #[test]
    fn test_vouch_exists() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert!(!client.vouch_exists(&voucher, &borrower));
        client.vouch(&voucher, &borrower, &1_000_000);
        assert!(client.vouch_exists(&voucher, &borrower));
    }

    // ── Loan Status Tests ─────────────────────────────────────────────────────

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
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(client.loan_status(&borrower), LoanStatus::Active);
    }

    #[test]
    fn test_loan_status_repaid() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        assert_eq!(client.loan_status(&borrower), LoanStatus::Repaid);
    }

    #[test]
    fn test_loan_status_defaulted() {
        let env = Env::default();
        let (contract_id, _, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.slash(&borrower);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower);
        assert_eq!(client.loan_status(&borrower), LoanStatus::Defaulted);
    }

    #[test]
    fn test_is_initialized() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let admins = single_admin_signers(&env, &admin);
        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert!(!client.is_initialized());
        client.initialize(&admin, &admins, &1, &token_id.address());
        assert!(client.is_initialized());
    }

    #[test]
    fn test_get_token_returns_token_address() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_token(), token_addr);
    }

    // ── Multisig Tests ────────────────────────────────────────────────────────

    #[test]
    fn test_multisig_threshold_allows_admin_operation() {
        let env = Env::default();
        let (contract_id, _token_addr, admin_one, admin_two, _admin_three, _borrower, _voucher) =
            setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one.clone(), admin_two.clone()]);

        client.pause(&signers);

        assert!(client.get_paused());
        assert_eq!(client.get_admin_threshold(), 2);
    }

    #[test]
    #[should_panic(expected = "insufficient admin approvals")]
    fn test_multisig_threshold_rejects_too_few_signers() {
        let env = Env::default();
        let (contract_id, _token_addr, admin_one, _admin_two, _admin_three, _borrower, _voucher) =
            setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = single_admin_signers(&env, &admin_one);

        client.pause(&signers);
    }

    #[test]
    #[should_panic(expected = "unauthorized admin signer")]
    fn test_multisig_threshold_rejects_non_admin_signer() {
        let env = Env::default();
        let (contract_id, _token_addr, admin_one, _admin_two, _admin_three, _borrower, _voucher) =
            setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let outsider = Address::generate(&env);
        let signers = address_vec(&env, &[admin_one, outsider]);

        client.pause(&signers);
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
        let (contract_id, _, admin, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.yield_bps = 300;
        cfg.slash_bps = 6000;
        cfg.loan_duration = 60 * 60 * 24 * 7;
        client.set_config(&admin_signers, &cfg);

        let updated = client.get_config();
        assert_eq!(updated.yield_bps, 300);
        assert_eq!(updated.slash_bps, 6000);
        assert_eq!(updated.loan_duration, 60 * 60 * 24 * 7);
    }

    #[test]
    fn test_config_yield_bps_applied_on_repay() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.yield_bps = 500;
        client.set_config(&admin_signers, &cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower, &500_000);

        assert_eq!(token.balance(&voucher), 10_025_000);
    }

    #[test]
    fn test_config_slash_bps_applied_on_slash() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.slash_bps = 2500;
        client.set_config(&admin_signers, &cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower);

        assert_eq!(token.balance(&voucher), 9_750_000);
        assert_eq!(client.get_slash_treasury(), 250_000);
    }

    #[test]
    #[should_panic(expected = "slash_bps must be 1-10000")]
    fn test_set_config_slash_bps_above_10000_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.slash_bps = 10_001;
        client.set_config(&admin_signers, &cfg);
    }

    #[test]
    fn test_set_config_slash_bps_at_boundary_10000_accepted() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.slash_bps = 10_000;
        client.set_config(&admin_signers, &cfg);

        assert_eq!(client.get_config().slash_bps, 10_000);
    }

    // ── Protocol Fee Tests ────────────────────────────────────────────────────

    #[test]
    fn test_set_protocol_fee() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        assert_eq!(client.get_protocol_fee(), 0);

        client.set_protocol_fee(&admin_signers, &200);
        assert_eq!(client.get_protocol_fee(), 200);
    }

    #[test]
    #[should_panic(expected = "fee_bps must not exceed 10000")]
    fn test_set_protocol_fee_exceeds_max() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.set_protocol_fee(&admin_signers, &10_001);
    }

    // ── Reputation NFT Tests ──────────────────────────────────────────────────

    fn setup_with_reputation(env: &Env) -> (Address, Address, Address, Address, Address, Address) {
        let (contract_id, token_addr, admin, borrower, voucher) = setup(env);
        let client = QuorumCreditContractClient::new(env, &contract_id);
        let admin_signers = single_admin_signers(env, &admin);

        let nft_id = env.register_contract(None, reputation::ReputationNftContract);
        reputation::ReputationNftContractClient::new(env, &nft_id).initialize(&contract_id);
        client.set_reputation_nft(&admin_signers, &nft_id);

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
        client.repay(&borrower, &500_000);

        assert!(client.is_eligible(&borrower, &1_000_000));
        assert_eq!(client.get_reputation(&borrower), 1);
        assert_eq!(nft.balance(&borrower), 1);
    }

    #[test]
    fn test_slash_burns_reputation() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher, nft_id) =
            setup_with_reputation(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let nft = reputation::ReputationNftContractClient::new(&env, &nft_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        assert_eq!(nft.balance(&borrower), 1);

        let borrower2 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &2_000_000);

        nft.mint(&borrower2);
        assert_eq!(nft.balance(&borrower2), 1);

        client.vouch(&voucher2, &borrower2, &1_000_000);
        client.request_loan(&borrower2, &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower2);

        assert_eq!(client.get_reputation(&borrower2), 0);
        assert_eq!(nft.balance(&borrower2), 0);
    }

    #[test]
    fn test_slash_burn_floors_at_zero() {
        let env = Env::default();
        let (contract_id, _token, admin, borrower, voucher, _nft_id) = setup_with_reputation(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower);

        // voucher started with 10_000_000, staked 1_000_000, gets back 750_000
        assert_eq!(token.balance(&voucher), 9_750_000);
    }

    #[test]
    fn test_get_reputation_without_nft_returns_zero() {
        let env = Env::default();
        let (contract_id, _token, _admin, borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_reputation(&borrower), 0);


    }

    // ── Repayment Count Tests ─────────────────────────────────────────────────

    #[test]
    fn test_repayment_count_tracks_successful_repayments() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        assert_eq!(client.repayment_count(&borrower), 0);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        assert_eq!(client.repayment_count(&borrower), 1);

        client.request_loan(&borrower, &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        assert_eq!(client.repayment_count(&borrower), 2);

        let borrower2 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);

        assert_eq!(client.repayment_count(&borrower2), 0);
        client.vouch(&voucher2, &borrower2, &1_000_000);
        client.request_loan(&borrower2, &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower2);
        assert_eq!(client.repayment_count(&borrower2), 0);
    }

    // ── Loan Pool Tests ───────────────────────────────────────────────────────

    #[test]
    fn test_create_loan_pool_success() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let borrower1 = Address::generate(&env);
        let borrower2 = Address::generate(&env);
        let voucher1 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher1, &10_000_000);
        token_admin.mint(&voucher2, &10_000_000);
        client.vouch(&voucher1, &borrower1, &2_000_000);
        client.vouch(&voucher2, &borrower2, &2_000_000);

        let mut borrowers = Vec::new(&env);
        borrowers.push_back(borrower1.clone());
        borrowers.push_back(borrower2.clone());
        let mut amounts = Vec::new(&env);
        amounts.push_back(500_000i128);
        amounts.push_back(300_000i128);

        let pool_id = client.create_loan_pool(&admin_signers, &borrowers, &amounts);
        assert_eq!(pool_id, 1);

        let pool = client.get_loan_pool(&pool_id).unwrap();
        assert_eq!(pool.pool_id, 1);
        assert_eq!(pool.total_disbursed, 800_000);
        assert_eq!(pool.borrowers.len(), 2);

        let loan1 = client.get_loan(&borrower1).unwrap();
        assert_eq!(loan1.amount, 500_000);
        assert!(!loan1.repaid);
        assert!(!loan1.defaulted);

        let loan2 = client.get_loan(&borrower2).unwrap();
        assert_eq!(loan2.amount, 300_000);

        assert_eq!(token.balance(&borrower1), 500_000);
        assert_eq!(token.balance(&borrower2), 300_000);
    }

    #[test]
    fn test_create_loan_pool_increments_pool_id() {
        let env = Env::default();
        let (contract_id, token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        assert_eq!(client.get_loan_pool_count(), 0);

        let b1 = Address::generate(&env);
        let v1 = Address::generate(&env);
        token_admin.mint(&v1, &10_000_000);
        client.vouch(&v1, &b1, &2_000_000);
        let mut bs1 = Vec::new(&env);
        bs1.push_back(b1);
        let mut am1 = Vec::new(&env);
        am1.push_back(500_000i128);
        assert_eq!(client.create_loan_pool(&admin_signers, &bs1, &am1), 1);

        let b2 = Address::generate(&env);
        let v2 = Address::generate(&env);
        token_admin.mint(&v2, &10_000_000);
        client.vouch(&v2, &b2, &2_000_000);
        let mut bs2 = Vec::new(&env);
        bs2.push_back(b2);
        let mut am2 = Vec::new(&env);
        am2.push_back(500_000i128);
        assert_eq!(client.create_loan_pool(&admin_signers, &bs2, &am2), 2);

        assert_eq!(client.get_loan_pool_count(), 2);
    }

    #[test]
    fn test_create_loan_pool_length_mismatch_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut borrowers = Vec::new(&env);
        borrowers.push_back(Address::generate(&env));
        let amounts: Vec<i128> = Vec::new(&env);

        let result = client.try_create_loan_pool(&admin_signers, &borrowers, &amounts);
        assert_eq!(result, Err(Ok(ContractError::PoolLengthMismatch)));
    }

    #[test]
    fn test_create_loan_pool_empty_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let borrowers: Vec<Address> = Vec::new(&env);
        let amounts: Vec<i128> = Vec::new(&env);

        let result = client.try_create_loan_pool(&admin_signers, &borrowers, &amounts);
        assert_eq!(result, Err(Ok(ContractError::PoolEmpty)));
    }

    #[test]
    fn test_create_loan_pool_rejects_active_loan_borrower() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &2_000_000);
        client.request_loan(&borrower, &500_000, &2_000_000);

        let mut borrowers = Vec::new(&env);
        borrowers.push_back(borrower);
        let mut amounts = Vec::new(&env);
        amounts.push_back(500_000i128);

        let result = client.try_create_loan_pool(&admin_signers, &borrowers, &amounts);
        assert_eq!(result, Err(Ok(ContractError::PoolBorrowerActiveLoan)));
    }

    #[test]
    fn test_create_loan_pool_insufficient_funds() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let borrower = Address::generate(&env);
        let voucher = Address::generate(&env);
        let admins = single_admin_signers(&env, &admin);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let token_admin = StellarAssetClient::new(&env, &token_id.address());
        token_admin.mint(&voucher, &10_000_000);

        let contract_id = env.register_contract(None, QuorumCreditContract);

        QuorumCreditContractClient::new(&env, &contract_id).initialize(
            &admin,
            &admins,
            &1,
            &token_id.address(),
        );

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &5_000_000);

        let mut borrowers = Vec::new(&env);
        borrowers.push_back(borrower);
        let mut amounts = Vec::new(&env);
        amounts.push_back(6_000_000i128);

        let result = client.try_create_loan_pool(&admins, &borrowers, &amounts);
        assert_eq!(result, Err(Ok(ContractError::PoolInsufficientFunds)));
    }

    #[test]
    fn test_get_loan_pool_returns_none_for_missing_id() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert!(client.get_loan_pool(&999u64).is_none());
    }
}
