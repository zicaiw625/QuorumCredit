#![allow(unused)]

use soroban_sdk::{contracttype, Address, Vec};

// ── Constants ─────────────────────────────────────────────────────────────────

pub const DEFAULT_YIELD_BPS: i128 = 200;
pub const DEFAULT_SLASH_BPS: i128 = 5000;
pub const DEFAULT_MIN_YIELD_STAKE: i128 = 50;
pub const DEFAULT_REFERRAL_BONUS_BPS: u32 = 100; // 1% of loan amount
pub const MIN_VOUCH_AGE: u64 = 60; // 1 minute
pub const DEFAULT_MAX_VOUCHERS: u32 = 100;
pub const DEFAULT_MIN_LOAN_AMOUNT: i128 = 100_000;
pub const DEFAULT_LOAN_DURATION: u64 = 30 * 24 * 60 * 60;
pub const DEFAULT_MAX_LOAN_TO_STAKE_RATIO: u32 = 150;
pub const DEFAULT_VOUCH_COOLDOWN_SECS: u64 = 24 * 60 * 60; // 24 hours
pub const DEFAULT_MAX_VOUCHERS_PER_BORROWER: u32 = 50;
pub const TIMELOCK_DELAY: u64 = 24 * 60 * 60;
pub const TIMELOCK_EXPIRY: u64 = 72 * 60 * 60;

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
    Loan(u64),                   // loan_id → LoanRecord
    ActiveLoan(Address),         // borrower → active loan_id
    LatestLoan(Address),         // borrower → latest loan_id
    Vouches(Address),            // borrower → Vec<VouchRecord>
    VoucherHistory(Address),     // voucher → Vec<Address> (borrowers backed)
    Config,                      // Config struct: all configurable protocol parameters
    Deployer,                    // Address that deployed the contract; guards initialize
    SlashTreasury,               // i128 accumulated slashed funds
    Paused,                      // bool: true when contract is paused
    BorrowerList,                // Vec<Address> of all borrowers who have ever requested a loan
    ReputationNft,               // Address of the ReputationNftContract
    MinStake,                    // i128 minimum stake amount per vouch
    MaxLoanAmount,               // i128 maximum individual loan size (0 = no cap)
    MinVouchers,     // u32 minimum number of distinct vouchers required (0 = no minimum)
    LoanCounter,     // u64: monotonically increasing loan ID counter
    LoanPool(u64),   // pool_id → LoanPoolRecord
    LoanPoolCounter, // u64: monotonically increasing pool ID counter
    PendingAdmin,    // Address of the pending admin (two-step transfer)
    RepaymentCount(Address), // borrower → u32 total successful repayments
    LoanCount(Address), // borrower → u32 total historical loans disbursed
    DefaultCount(Address), // borrower → u32 total defaults (slash + auto_slash + claim_expired)
    ProtocolFeeBps,  // u32: protocol fee in basis points
    FeeTreasury,     // Address: recipient of collected protocol fees
    LastVouchTimestamp(Address), // voucher → u64 last vouch timestamp
    VouchCooldownSecs,           // u64 cooldown between vouch calls (default 24 hours)
    Timelock(u64),   // proposal_id → TimelockProposal
    TimelockCounter, // u64 monotonically increasing proposal ID
    Blacklisted(Address), // borrower → bool permanently banned
    VoucherWhitelist(Address), // voucher → bool allowed to vouch
    ExtensionConsents(Address), // borrower → Vec<Address> vouchers who consented to extension
    SlashVote(Address), // borrower → SlashVoteRecord
    SlashVoteQuorum, // u32 quorum in basis points (e.g. 5000 = 50%)
    ReferredBy(Address), // borrower → Address of referrer
    ReferralBonusBps, // u32 referral bonus in basis points (default 100 = 1%)
    MaxVouchersPerBorrower, // u32 maximum number of vouchers per borrower (default 50)
}

// ── Governance ────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub struct SlashVoteRecord {
    pub approve_stake: i128,  // total stake voting to approve slash
    pub reject_stake: i128,   // total stake voting to reject slash
    pub voters: Vec<Address>, // addresses that have already voted
    pub executed: bool,       // true once slash has been auto-executed
}

// ── Config ────────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub struct Config {
    pub admins: Vec<Address>,
    pub admin_threshold: u32,
    pub token: Address,
    pub allowed_tokens: Vec<Address>, // additional tokens accepted for loans/vouches
    pub yield_bps: i128,
    pub slash_bps: i128,
    pub max_vouchers: u32,
    pub min_loan_amount: i128,
    pub loan_duration: u64,
    pub max_loan_to_stake_ratio: u32,
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
    pub status: LoanStatus,
    pub created_at: u64,                   // ledger timestamp
    pub disbursement_timestamp: u64,       // ledger timestamp
    pub repayment_timestamp: Option<u64>,  // set once the loan is fully repaid
    pub deadline: u64,                     // repayment deadline (ledger timestamp)
    pub loan_purpose: soroban_sdk::String, // borrower-supplied purpose string
    pub token_address: Address,            // token used for this loan
}

#[contracttype]
#[derive(Clone)]
pub struct VouchRecord {
    pub voucher: Address,
    pub stake: i128,          // in stroops
    pub vouch_timestamp: u64, // ledger timestamp when vouch was created; immutable after set
    pub token: Address,       // token this stake is denominated in
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
#[derive(Clone)]
pub struct TimelockProposal {
    pub id: u64,
    pub action: TimelockAction,
    pub proposer: Address,
    pub eta: u64,
    pub executed: bool,
    pub cancelled: bool,
}

#[contracttype]
#[derive(Clone)]
pub enum TimelockAction {
    Slash(Address),
    SetConfig(Config),
}
