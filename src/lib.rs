#![no_std]

use soroban_sdk::{
    contract, contractimpl, panic_with_error, symbol_short, Address, BytesN, Env, Vec,
};

// Module declarations
pub mod admin;
pub mod errors;
pub mod helpers;
pub mod loan;
pub mod reputation;
<<<<<<< emitevents
pub mod types;
pub mod vouch;

// Re-exports for external use
pub use errors::ContractError;
pub use types::*;
=======
use reputation::ReputationNftExternalClient;

// ── Constants (defaults only) ─────────────────────────────────────────────────

const DEFAULT_YIELD_BPS: i128 = 200;
const DEFAULT_SLASH_BPS: i128 = 5000;
/// Minimum stake (in stroops) required for a vouch to earn non-zero yield.
/// At 200 bps (2%), a stake must be at least 50 stroops so that
/// `stake * 200 / 10_000 >= 1`. Stakes below this threshold would silently
/// truncate to zero yield due to integer division.
const DEFAULT_MIN_YIELD_STAKE: i128 = 50;
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
const DEFAULT_SLASH_CHALLENGE_WINDOW: u64 = 7 * 24 * 60 * 60;
const DEFAULT_VOUCH_COOLDOWN_SECS: u64 = 24 * 60 * 60; // 24 hours
/// Delay before a timelocked action can be executed (24 hours).
const TIMELOCK_DELAY: u64 = 24 * 60 * 60;
/// Window after eta during which the action can still be executed (72 hours).
const TIMELOCK_EXPIRY: u64 = 72 * 60 * 60;

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    InsufficientFunds = 1,
    /// Borrower already has an active (non-repaid, non-defaulted) loan.
    ActiveLoanExists = 2,
    /// Total vouched stake overflowed i128.
    StakeOverflow = 3,
    /// admin or token address must not be the zero address.
    ZeroAddress = 4,
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
    InvalidAmount = 14,
    InvalidStateTransition = 15,
    AlreadyInitialized = 16,
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
    Loan(u64),               // loan_id → LoanRecord
    ActiveLoan(Address),     // borrower → active loan_id
    LatestLoan(Address),     // borrower → latest loan_id
    Vouches(Address),        // borrower → Vec<VouchRecord>
    VoucherHistory(Address), // voucher → Vec<Address> (borrowers backed)
    Config,                  // Config struct: all configurable protocol parameters
    Deployer,                // Address that deployed the contract; guards initialize
    SlashTreasury,           // i128 accumulated slashed funds
    Paused,                  // bool: true when contract is paused
    BorrowerList,            // Vec<Address> of all borrowers who have ever requested a loan
    ReputationNft,           // Address of the ReputationNftContract
    MinStake,                // i128 minimum stake amount per vouch
    MaxLoanAmount,           // i128 maximum individual loan size (0 = no cap)
    MinVouchers,             // u32 minimum number of distinct vouchers required (0 = no minimum)
    LoanCounter,             // u64: monotonically increasing loan ID counter
    LoanPool(u64),           // pool_id → LoanPoolRecord
    LoanPoolCounter,         // u64: monotonically increasing pool ID counter
    PendingAdmin,            // Address of the pending admin (two-step transfer)
    RepaymentCount(Address), // borrower → u32 total successful repayments
    LoanCount(Address),      // borrower → u32 total historical loans disbursed
    DefaultCount(Address),   // borrower → u32 total defaults (slash + auto_slash + claim_expired)
    ProtocolFeeBps,          // u32: protocol fee in basis points
    FeeTreasury,             // Address: recipient of collected protocol fees
    LastVouchTimestamp(Address), // voucher → u64 last vouch timestamp
    Timelock(u64),               // proposal_id → TimelockProposal
    TimelockCounter,             // u64 monotonically increasing proposal ID
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
    /// Minimum stake in stroops required for a vouch to earn non-zero yield.
    /// Vouches below this threshold are rejected to prevent silent yield truncation.
    /// At the default 200 bps yield rate, the minimum is 50 stroops
    /// (50 * 200 / 10_000 = 1 stroop of yield).
    pub min_yield_stake: i128,
    pub slash_challenge_window: u64,
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
            min_yield_stake: DEFAULT_MIN_YIELD_STAKE,
            slash_challenge_window: DEFAULT_SLASH_CHALLENGE_WINDOW,
        }
    }
    /// Grace period after deadline before auto_slash is allowed, in seconds (default 3 days).
    /// A value of 0 means slashing is allowed immediately after the deadline.
    pub grace_period: u64,
}

// ── Data Types ────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub struct LoanRecord {
    pub id: u64,
    pub borrower: Address,
    pub co_borrowers: Vec<Address>,
    pub amount: i128,        // total loan principal in stroops
    pub amount_repaid: i128, // cumulative repayments received so far (principal + yield)
    pub total_yield: i128,   // yield owed to vouchers, locked in at disbursement
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

#[contracttype]
#[derive(Clone)]
pub struct LoanPoolRecord {
    pub pool_id: u64,
    pub borrowers: Vec<Address>,
    pub amounts: Vec<i128>,
    pub created_at: u64,
    pub total_disbursed: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChallengeStatus {
    Pending,       
    Challenged,
    Resolved,
    Finalized,
}

#[contracttype]
#[derive(Clone)]
pub struct SlashChallengeRecord {
    pub borrower: Address,
    pub initiated_at: u64,
    pub challenge_deadline: u64,
    pub status: ChallengeStatus,
    pub reason: soroban_sdk::Symbol,
// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns true if the address is the all-zeros account or contract address.
fn is_zero_address(env: &Env, addr: &Address) -> bool {
    // Stellar zero account: all-zero 32-byte ed25519 key
    let zero_account = Address::from_string(&soroban_sdk::String::from_str(
        env,
        "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
    ));
    // Stellar zero contract: all-zero 32-byte contract hash
    let zero_contract = Address::from_string(&soroban_sdk::String::from_str(
        env,
        "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
    ));
    addr == &zero_account || addr == &zero_contract
}
>>>>>>> main

use helpers::{config, validate_admin_config};
use reputation::ReputationNftExternalClient;

#[contract]
pub struct QuorumCreditContract;

#[contractimpl]
impl QuorumCreditContract {
    /// One-time initialisation: set admins, XLM token address, and default config.
    pub fn initialize(
        env: Env,
        deployer: Address,
        admins: Vec<Address>,
        admin_threshold: u32,
        token: Address,
    ) {
        deployer.require_auth();

        if env.storage().instance().has(&DataKey::Config) {
            panic_with_error!(&env, ContractError::AlreadyInitialized);
        }

        validate_admin_config(&admins, admin_threshold);

        env.storage().instance().set(&DataKey::Deployer, &deployer);
        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admins: admins.clone(),
                admin_threshold,
                token: token.clone(),
                yield_bps: DEFAULT_YIELD_BPS,
                slash_bps: DEFAULT_SLASH_BPS,
                max_vouchers: DEFAULT_MAX_VOUCHERS,
                min_loan_amount: DEFAULT_MIN_LOAN_AMOUNT,
                loan_duration: DEFAULT_LOAN_DURATION,
                max_loan_to_stake_ratio: DEFAULT_MAX_LOAN_TO_STAKE_RATIO,
                grace_period: 0,
            },
        );

        env.events().publish(
            (symbol_short!("contract"), symbol_short!("init")),
            (deployer.clone(), admins, admin_threshold, token),
        );
    }

    // ── Vouch Functions ───────────────────────────────────────────────────────

    pub fn vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
        stake: i128,
    ) -> Result<(), ContractError> {
        vouch::vouch(env, voucher, borrower, stake)
    }

    pub fn batch_vouch(
        env: Env,
        voucher: Address,
        borrowers: Vec<Address>,
        stakes: Vec<i128>,
    ) -> Result<(), ContractError> {
        vouch::batch_vouch(env, voucher, borrowers, stakes)
    }

    pub fn increase_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        additional: i128,
    ) -> Result<(), ContractError> {
        vouch::increase_stake(env, voucher, borrower, additional)
    }

    pub fn decrease_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        amount: i128,
    ) -> Result<(), ContractError> {
        vouch::decrease_stake(env, voucher, borrower, amount)
    }

    pub fn withdraw_vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        vouch::withdraw_vouch(env, voucher, borrower)
    }

    pub fn transfer_vouch(
        env: Env,
        from: Address,
        to: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        vouch::transfer_vouch(env, from, to, borrower)
    }

    // ── Loan Functions ────────────────────────────────────────────────────────

    pub fn request_loan(
        env: Env,
        borrower: Address,
        amount: i128,
        threshold: i128,
    ) -> Result<(), ContractError> {
        loan::request_loan(env, borrower, amount, threshold)
    }

    pub fn repay(env: Env, borrower: Address, payment: i128) -> Result<(), ContractError> {
        loan::repay(env, borrower, payment)
    }

    // ── Admin Functions ───────────────────────────────────────────────────────

    pub fn add_admin(env: Env, admin_signers: Vec<Address>, new_admin: Address) {
        admin::add_admin(env, admin_signers, new_admin)
    }

    pub fn remove_admin(env: Env, admin_signers: Vec<Address>, admin_to_remove: Address) {
        admin::remove_admin(env, admin_signers, admin_to_remove)
    }

    pub fn rotate_admin(
        env: Env,
        admin_signers: Vec<Address>,
        old_admin: Address,
        new_admin: Address,
    ) {
        admin::rotate_admin(env, admin_signers, old_admin, new_admin)
    }

    pub fn set_admin_threshold(env: Env, admin_signers: Vec<Address>, new_threshold: u32) {
        admin::set_admin_threshold(env, admin_signers, new_threshold)
    }

    pub fn set_protocol_fee(env: Env, admin_signers: Vec<Address>, fee_bps: u32) {
        admin::set_protocol_fee(env, admin_signers, fee_bps)
    }

    pub fn whitelist_voucher(env: Env, admin_signers: Vec<Address>, voucher: Address) {
        admin::whitelist_voucher(env, admin_signers, voucher)
    }

    pub fn set_fee_treasury(env: Env, admin_signers: Vec<Address>, treasury: Address) {
        admin::set_fee_treasury(env, admin_signers, treasury)
    }

    pub fn upgrade(env: Env, admin_signers: Vec<Address>, new_wasm_hash: BytesN<32>) {
        admin::upgrade(env, admin_signers, new_wasm_hash)
    }

    pub fn pause(env: Env, admin_signers: Vec<Address>) {
        admin::pause(env, admin_signers)
    }

    pub fn unpause(env: Env, admin_signers: Vec<Address>) {
        admin::unpause(env, admin_signers)
    }

    pub fn blacklist(env: Env, admin_signers: Vec<Address>, borrower: Address) {
        admin::blacklist(env, admin_signers, borrower)
    }

    pub fn set_config(env: Env, admin_signers: Vec<Address>, config: Config) {
        admin::set_config(env, admin_signers, config)
    }

    pub fn set_reputation_nft(env: Env, admin_signers: Vec<Address>, nft_contract: Address) {
        admin::set_reputation_nft(env, admin_signers, nft_contract)
    }

    pub fn set_min_stake(env: Env, admin_signers: Vec<Address>, amount: i128) {
        admin::set_min_stake(env, admin_signers, amount)
    }

    pub fn set_max_loan_amount(env: Env, admin_signers: Vec<Address>, amount: i128) {
        admin::set_max_loan_amount(env, admin_signers, amount)
    }

    pub fn set_min_vouchers(env: Env, admin_signers: Vec<Address>, count: u32) {
        admin::set_min_vouchers(env, admin_signers, count)
    }

    pub fn set_max_loan_to_stake_ratio(env: Env, admin_signers: Vec<Address>, ratio: u32) {
        admin::set_max_loan_to_stake_ratio(env, admin_signers, ratio)
    }

    // ── View Functions ────────────────────────────────────────────────────────

    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&DataKey::Config)
    }

    pub fn get_token(env: Env) -> Address {
        config(&env).token
    }

    pub fn get_admins(env: Env) -> Vec<Address> {
        admin::get_admins(env)
    }

    pub fn get_admin_threshold(env: Env) -> u32 {
        admin::get_admin_threshold(env)
    }

    pub fn get_slash_treasury_balance(env: Env) -> i128 {
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
        loan::loan_status(env, borrower)
    }

    pub fn vouch_exists(env: Env, voucher: Address, borrower: Address) -> bool {
        vouch::vouch_exists(env, voucher, borrower)
    }

    pub fn is_whitelisted(env: Env, voucher: Address) -> bool {
        admin::is_whitelisted(env, voucher)
    }

    pub fn get_loan(env: Env, borrower: Address) -> Option<LoanRecord> {
        loan::get_loan(env, borrower)
    }

    pub fn get_loan_by_id(env: Env, loan_id: u64) -> Option<LoanRecord> {
        loan::get_loan_by_id(env, loan_id)
    }

    pub fn get_vouches(env: Env, borrower: Address) -> Option<Vec<VouchRecord>> {
        env.storage().persistent().get(&DataKey::Vouches(borrower))
    }

    pub fn is_eligible(env: Env, borrower: Address, threshold: i128) -> bool {
        loan::is_eligible(env, borrower, threshold)
    }

    pub fn get_contract_balance(env: Env) -> i128 {
        helpers::token(&env).balance(&env.current_contract_address())
    }

    pub fn voucher_history(env: Env, voucher: Address) -> Vec<Address> {
        vouch::voucher_history(env, voucher)
    }

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

    pub fn total_vouched(env: Env, borrower: Address) -> i128 {
        vouch::total_vouched(env, borrower)
    }

    pub fn repayment_count(env: Env, borrower: Address) -> u32 {
        loan::repayment_count(env, borrower)
    }

    pub fn loan_count(env: Env, borrower: Address) -> u32 {
        loan::loan_count(env, borrower)
    }

    pub fn default_count(env: Env, borrower: Address) -> u32 {
        loan::default_count(env, borrower)
    }

    pub fn get_protocol_fee(env: Env) -> u32 {
        admin::get_protocol_fee(env)
    }

    pub fn get_fee_treasury(env: Env) -> Option<Address> {
        admin::get_fee_treasury(env)
    }

    pub fn is_blacklisted(env: Env, borrower: Address) -> bool {
        admin::is_blacklisted(env, borrower)
    }

    pub fn get_min_stake(env: Env) -> i128 {
        admin::get_min_stake(env)
    }

    pub fn get_max_loan_amount(env: Env) -> i128 {
        admin::get_max_loan_amount(env)
    }

    pub fn get_min_vouchers(env: Env) -> u32 {
        admin::get_min_vouchers(env)
    }

    pub fn get_max_loan_to_stake_ratio(env: Env) -> u32 {
<<<<<<< emitevents
        admin::get_max_loan_to_stake_ratio(env)
=======
        Self::config(&env).max_loan_to_stake_ratio
    }

    /// Admin updates configurable protocol parameters.
    pub fn set_config(env: Env, admin_signers: Vec<Address>, config: Config) {
        Self::require_admin_approval(&env, &admin_signers);
        assert!(config.yield_bps >= 0, "yield_bps must be non-negative");
        assert!(
            config.slash_bps > 0 && config.slash_bps <= 10_000,
            "slash_bps must be 1-10000"
        );
        assert!(config.max_vouchers > 0, "max_vouchers must be greater than zero");
        assert!(config.min_loan_amount > 0, "min_loan_amount must be greater than zero");
        assert!(config.loan_duration > 0, "loan_duration must be greater than zero");
        assert!(
            config.max_loan_to_stake_ratio > 0,
            "max_loan_to_stake_ratio must be greater than zero"
        );
        assert!(config.slash_challenge_window > 0, "challenge window must be positive");
        Self::validate_admin_config(&config.admins, config.admin_threshold);
        // grace_period of 0 is valid — means no grace period, slash allowed immediately after deadline.
        env.storage().instance().set(&DataKey::Config, &config);
        env.events().publish(
            (symbol_short!("admin"), symbol_short!("config")),
            (admin_signers.get(0).unwrap(), env.ledger().timestamp()),
        );
>>>>>>> main
    }

    pub fn get_config(env: Env) -> Config {
        admin::get_config(env)
    }
}
