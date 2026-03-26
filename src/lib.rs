#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, token, Address, BytesN, Env,
    Vec,
};

pub mod reputation;
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
    VouchCooldownActive = 14,
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
    BorrowerList,            // Vec<Address> of all borrowers who have ever requested a loan
    ReputationNft,           // Address of the ReputationNftContract
    MinStake,                // i128 minimum stake amount per vouch
    MaxLoanAmount,           // i128 maximum individual loan size (0 = no cap)
    MinVouchers,             // u32 minimum number of distinct vouchers required (0 = no minimum)
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
    /// Grace period after deadline before auto_slash is allowed, in seconds (default 3 days).
    /// A value of 0 means slashing is allowed immediately after the deadline.
    pub grace_period: u64,
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
            grace_period: DEFAULT_GRACE_PERIOD,
        }
    }
}

// ── Data Types ────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub struct LoanRecord {
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

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct QuorumCreditContract;

#[contractimpl]
impl QuorumCreditContract {
    /// One-time initialisation: set admins, XLM token address, and default config.
    ///
    /// `deployer` must be the address that deployed this contract and must
    /// sign this transaction. This prevents front-running attacks where an
    /// observer of the deployment transaction calls `initialize` first with
    /// their own admin address before the legitimate deployer can do so.
    pub fn initialize(
        env: Env,
        deployer: Address,
        admin: Address,
        token: Address,
    ) -> Result<(), ContractError> {
        // Require the deployer's signature — only they can authorise this call.
    /// sign this transaction. This prevents front-running attacks.
    pub fn initialize(
        env: Env,
        deployer: Address,
        admins: Vec<Address>,
        admin_threshold: u32,
        token: Address,
    ) {
        deployer.require_auth();

        assert!(
            !env.storage().instance().has(&DataKey::Config),
            "already initialized"
        );

        if is_zero_address(&env, &admin) || is_zero_address(&env, &token) {
            return Err(ContractError::ZeroAddress);
        }

        env.storage().instance().set(&DataKey::Deployer, &deployer);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
        Ok(())
        Self::validate_admin_config(&admins, admin_threshold);

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
                min_yield_stake: DEFAULT_MIN_YIELD_STAKE,
                vouch_cooldown_secs: DEFAULT_VOUCH_COOLDOWN_SECS,
            },
        );
    }

    /// Stake XLM to vouch for a borrower.
    ///
    /// Sybil resistance is enforced here via two config parameters:
    /// - `min_stake`: each voucher must lock a meaningful economic stake.
    /// - `min_vouchers` (enforced at loan request): a minimum number of
    ///   *distinct* vouchers must back the borrower before a loan is disbursed.
    pub fn vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
        stake: i128,
    ) -> Result<(), ContractError> {
        voucher.require_auth();
        Self::require_not_paused(&env)?;
        Self::do_vouch(&env, voucher, borrower, stake)
    }

    fn do_vouch(
        env: &Env,
        voucher: Address,
        borrower: Address,
        stake: i128,
    ) -> Result<(), ContractError> {
        // Validate numeric input: stake must be strictly positive.
        Self::require_positive_amount(env, stake)?;

        assert!(voucher != borrower, "voucher cannot vouch for self");
        assert!(stake > 0, "stake must be greater than zero");

        let cfg = Self::config(&env);

        // Sybil resistance: enforce minimum stake per vouch.
        let min_stake: i128 = env.storage().instance().get(&DataKey::MinStake).unwrap_or(0);
        if min_stake > 0 && stake < min_stake {
            return Err(ContractError::MinStakeNotMet);
        }

        // Enforce minimum yield stake: reject stakes that would produce zero yield
        // due to integer division truncation (stake * yield_bps / 10_000 == 0).
        let cfg = Self::config(env);
        assert!(
            stake >= cfg.min_yield_stake,
            "stake too small: would produce zero yield due to integer truncation"
        );

        // Rate limiting: enforce cooldown between vouch calls from the same address.
        if cfg.vouch_cooldown_secs > 0 {
            let now = env.ledger().timestamp();
            let last: u64 = env
                .storage()
                .persistent()
                .get(&DataKey::LastVouchTimestamp(voucher.clone()))
                .unwrap_or(0);
            if last > 0 && now < last + cfg.vouch_cooldown_secs {
                return Err(ContractError::VouchCooldownActive);
            }
        }

        let mut vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(env));

        // Reject duplicate vouch before any state mutation or transfer.
        for v in vouches.iter() {
            if v.voucher == voucher {
                return Err(ContractError::DuplicateVouch);
            }
        }

        // Reject vouch if the borrower already has an active loan — the stake
        // would be locked with no effect on the existing loan (fixes issue #13).
        if Self::has_active_loan(&env, &borrower) {
            return Err(ContractError::BorrowerHasActiveLoan);
        }

        assert!(
            vouches.len() < Self::config(env).max_vouchers,
            "maximum vouchers per loan exceeded"
        );

        // Transfer stake from voucher into the contract.
        let token = Self::token(env);
        token.transfer(&voucher, &env.current_contract_address(), &stake);

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
        });
        env.storage()
            .persistent()
            .set(&DataKey::Vouches(borrower.clone()), &vouches);

        // Record the timestamp of this vouch for rate limiting.
        env.storage()
            .persistent()
            .set(&DataKey::LastVouchTimestamp(voucher.clone()), &env.ledger().timestamp());

        env.events().publish(
            (symbol_short!("vouch"), symbol_short!("added")),
            (voucher, borrower, stake),
        );

        Ok(())
    }

    /// Vouch for multiple borrowers in a single transaction.
    /// `borrowers` and `stakes` must have the same length.
    /// Each entry is processed identically to a single `vouch` call —
    /// any failure (duplicate, min-stake, paused, etc.) aborts the whole batch.
    pub fn batch_vouch(
        env: Env,
        voucher: Address,
        borrowers: Vec<Address>,
        stakes: Vec<i128>,
    ) -> Result<(), ContractError> {
        voucher.require_auth();
        Self::require_not_paused(&env)?;

        assert!(
            borrowers.len() == stakes.len(),
            "borrowers and stakes length mismatch"
        );
        assert!(!borrowers.is_empty(), "batch cannot be empty");

        for i in 0..borrowers.len() {
            let borrower = borrowers.get(i).unwrap();
            let stake = stakes.get(i).unwrap();
            Self::do_vouch(&env, voucher.clone(), borrower, stake)?;
        }

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
        Self::token_client(&env).transfer(&voucher, &env.current_contract_address(), &additional);

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

        if env
            .storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::Blacklisted(borrower.clone()))
            .unwrap_or(false)
        {
            return Err(ContractError::Blacklisted);
        }

        let cfg = Self::config(&env);

        assert!(
            amount >= cfg.min_loan_amount,
            "loan amount must meet minimum threshold"
        );
        // Validate threshold is strictly positive.
        assert!(threshold > 0, "threshold must be greater than zero");

        // Enforce max loan amount cap if configured.
        let max_loan_amount: i128 = env.storage().instance().get(&DataKey::MaxLoanAmount).unwrap_or(0);
        if max_loan_amount > 0 && amount > max_loan_amount {
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

        let mut total_stake: i128 = 0;
        for v in vouches.iter() {
            total_stake = total_stake
                .checked_add(v.stake)
                .ok_or(ContractError::StakeOverflow)?;
        }
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

        // Enforce minimum vouch age: every vouch must be at least MIN_VOUCH_AGE seconds old.
        // This prevents a same-transaction (or same-block) vouch → request_loan attack.
        let now = env.ledger().timestamp();
        for v in vouches.iter() {
            if now < v.vouch_timestamp + MIN_VOUCH_AGE {
                return Err(ContractError::VouchTooRecent);
            }
        }

        // Check collateral ratio: amount must not exceed total_stake * ratio / 100
        let max_allowed_loan = total_stake * cfg.max_loan_to_stake_ratio as i128 / 100;
        assert!(
            amount <= max_allowed_loan,
            "loan amount exceeds maximum collateral ratio"
        );

        // Verify the contract holds enough XLM to cover the loan.
        let token = Self::token_client(&env);
        let contract_balance = token.balance(&env.current_contract_address());
        if contract_balance < amount {
            return Err(ContractError::InsufficientFunds);
        }

        let deadline = now + cfg.loan_duration;

        // Lock in the yield at disbursement time so rate changes mid-loan don't
        // affect what the borrower owes or what vouchers receive (fixes issue #15).
        let total_yield = amount * cfg.yield_bps / 10_000;

        env.storage().persistent().set(
            &DataKey::Loan(borrower.clone()),
            &LoanRecord {
                borrower: borrower.clone(),
                co_borrowers,
                amount,
                amount_repaid: 0,
                total_yield,
                repaid: false,
                defaulted: false,
                created_at: now,
                disbursement_timestamp: now,
                deadline,
            },
        );

        // Track total historical loan count for this borrower.
        let count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::LoanCount(borrower.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::LoanCount(borrower.clone()), &(count + 1));

        token.transfer(&env.current_contract_address(), &borrower, &amount);

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("disbursed")),
            (borrower.clone(), amount, deadline),
        );

        Ok(())
    }

    /// Borrower repays all or part of the loan.
    ///
    /// `payment` is the amount being paid in this call (in stroops). The total
    /// amount the borrower must repay is `loan.amount + loan.total_yield` —
    /// principal plus the yield owed to vouchers. Yield is locked in at
    /// disbursement so the borrower's obligation is fixed from day one.
    ///
    /// When cumulative `amount_repaid` reaches `amount + total_yield`, the loan
    /// is marked fully repaid and each voucher receives their stake back plus
    /// their proportional share of `total_yield`. Because yield comes entirely
    /// from the borrower's repayment, no pre-funded contract balance is consumed
    /// (fixes issue #15).
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
        }

        // Guard: only an active (non-repaid, non-defaulted) loan may be repaid.
        if loan.defaulted || loan.repaid {
            return Err(ContractError::NoActiveLoan);
        }

        // Block repayment after deadline — borrower must be auto-slashed instead.
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

        let token = Self::token(&env);

        // Collect this installment from the borrower (principal + yield portion).
        token.transfer(&borrower, &env.current_contract_address(), &payment);
        loan.amount_repaid += payment;

        if loan.amount_repaid >= total_owed {
            // Fully repaid — distribute stake + proportional yield to each voucher.
            // Yield comes entirely from what the borrower just paid in, so no
            // pre-funded contract balance is consumed.
            let vouches: Vec<VouchRecord> = env
                .storage()
                .persistent()
                .get(&DataKey::Vouches(borrower.clone()))
                .unwrap_or(Vec::new(&env));

            let total_stake: i128 = vouches.iter().map(|v| v.stake).sum();

            // Deduct protocol fee from the repaid principal before computing yield.
            let fee_bps: u32 = env
                .storage()
                .instance()
                .get(&DataKey::ProtocolFeeBps)
                .unwrap_or(0);
            let protocol_fee = loan.amount * fee_bps as i128 / 10_000;
            let distributable = loan.amount - protocol_fee;
            let total_yield = distributable * cfg.yield_bps / 10_000;

            // Pre-check contract balance covers all payouts before committing.
            let total_payout: i128 = vouches.iter().map(|v| {
                let yield_amount = v.stake * cfg.yield_bps / 10_000;
                v.stake + yield_amount
            }).sum();
            let contract_balance = token.balance(&env.current_contract_address());
            assert!(
                contract_balance >= total_payout + protocol_fee,
                "insufficient contract balance for yield distribution"
            );

            // Send protocol fee to treasury if configured.
            if protocol_fee > 0 {
                if let Some(treasury) = env
                    .storage()
                    .instance()
                    .get::<DataKey, Address>(&DataKey::FeeTreasury)
                {
                    token.transfer(&env.current_contract_address(), &treasury, &protocol_fee);
                }
            }

            // Return stake + yield to each voucher.
            for v in vouches.iter() {
                let voucher_yield = if total_stake > 0 {
                    loan.total_yield * v.stake / total_stake
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
                ReputationNftExternalClient::new(&env, &nft_addr).mint(&borrower);
            }

            env.events().publish(
                (symbol_short!("loan"), symbol_short!("repaid")),
                (borrower.clone(), loan.amount),
            );
        }

        // Persist the updated loan record.
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower.clone()), &loan);

        Ok(())
        
        
    }

    /// Admin marks a loan defaulted; slash_bps% of each voucher's stake is slashed.
    /// For non-emergency use, prefer propose_action(Slash) + execute_action for the timelock path.
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
            panic_with_error!(&env, ContractError::NoActiveLoan);
        }

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

        let dc: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::DefaultCount(borrower.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::DefaultCount(borrower.clone()), &(dc + 1));

        let token = Self::token_client(&env);
        let mut total_slashed: i128 = 0;
        for v in vouches.iter() {
            let slash_amount = v.stake * cfg.slash_bps / 10_000;
            let returned = v.stake - slash_amount;
            if returned > 0 {
                token.transfer(&env.current_contract_address(), &v.voucher, &returned);
            }
            total_slashed += slash_amount;
        }

        let treasury: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SlashTreasury)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::SlashTreasury, &(treasury + total_slashed));

        // Burn one reputation point if a reputation NFT contract is configured.
        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftExternalClient::new(&env, &nft_addr).burn(&borrower);
        }

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
            panic_with_error!(&env, ContractError::NoActiveLoan);
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

        let dc: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::DefaultCount(borrower.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::DefaultCount(borrower.clone()), &(dc + 1));

        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower));
    }

    /// Admin withdraws accumulated slashed funds to a recipient address.

    pub fn slash_treasury(env: Env, recipient: Address) {
        Self::require_admin(&env);
    }

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
        Self::token_client(&env).transfer(&env.current_contract_address(), &recipient, &amount);
        env.events().publish(
            (symbol_short!("admin"), symbol_short!("treasury")),
            (admin_signers.get(0).unwrap(), recipient, amount, env.ledger().timestamp()),
        );
    }

    // ── Timelock ──────────────────────────────────────────────────────────────

    /// Propose a timelocked admin action. Returns the proposal ID.
    /// The action can be executed after TIMELOCK_DELAY seconds have elapsed.
    pub fn propose_action(
        env: Env,
        admin_signers: Vec<Address>,
        action: TimelockAction,
    ) -> u64 {
        Self::require_admin_approval(&env, &admin_signers);

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TimelockCounter)
            .unwrap_or(0u64)
            .checked_add(1)
            .expect("timelock ID overflow");
        env.storage().instance().set(&DataKey::TimelockCounter, &id);

        let eta = env.ledger().timestamp() + TIMELOCK_DELAY;
        let proposer = admin_signers.get(0).unwrap();

        env.storage().persistent().set(
            &DataKey::Timelock(id),
            &TimelockProposal {
                id,
                action: action.clone(),
                proposer: proposer.clone(),
                eta,
                executed: false,
                cancelled: false,
            },
        );

        env.events().publish(
            (symbol_short!("tl"), symbol_short!("proposed")),
            (id, proposer, eta),
        );
        id
    }

    /// Execute a timelocked proposal once its eta has passed.
    pub fn execute_action(
        env: Env,
        admin_signers: Vec<Address>,
        proposal_id: u64,
    ) -> Result<(), ContractError> {
        Self::require_admin_approval(&env, &admin_signers);

        let mut proposal: TimelockProposal = env
            .storage()
            .persistent()
            .get(&DataKey::Timelock(proposal_id))
            .ok_or(ContractError::TimelockNotFound)?;

        assert!(!proposal.cancelled, "proposal cancelled");
        assert!(!proposal.executed, "proposal already executed");

        let now = env.ledger().timestamp();
        if now < proposal.eta {
            return Err(ContractError::TimelockNotReady);
        }
        if now > proposal.eta + TIMELOCK_EXPIRY {
            return Err(ContractError::TimelockExpired);
        }

        proposal.executed = true;
        env.storage()
            .persistent()
            .set(&DataKey::Timelock(proposal_id), &proposal);

        match proposal.action.clone() {
            TimelockAction::Slash(borrower) => {
                Self::do_slash(&env, borrower);
            }
            TimelockAction::SetConfig(config) => {
                env.storage().instance().set(&DataKey::Config, &config);
                env.events().publish(
                    (symbol_short!("admin"), symbol_short!("config")),
                    (admin_signers.get(0).unwrap(), env.ledger().timestamp()),
                );
            }
        }

        env.events().publish(
            (symbol_short!("tl"), symbol_short!("executed")),
            (proposal_id, admin_signers.get(0).unwrap(), now),
        );
        Ok(())
    }

    /// Cancel a pending timelocked proposal before it is executed.
    pub fn cancel_action(
        env: Env,
        admin_signers: Vec<Address>,
        proposal_id: u64,
    ) -> Result<(), ContractError> {
        Self::require_admin_approval(&env, &admin_signers);

        let mut proposal: TimelockProposal = env
            .storage()
            .persistent()
            .get(&DataKey::Timelock(proposal_id))
            .ok_or(ContractError::TimelockNotFound)?;

        assert!(!proposal.executed, "proposal already executed");
        assert!(!proposal.cancelled, "proposal already cancelled");

        proposal.cancelled = true;
        env.storage()
            .persistent()
            .set(&DataKey::Timelock(proposal_id), &proposal);

        env.events().publish(
            (symbol_short!("tl"), symbol_short!("cancelled")),
            (proposal_id, admin_signers.get(0).unwrap()),
        );
        Ok(())
    }

    /// Read a timelock proposal by ID.
    pub fn get_proposal(env: Env, proposal_id: u64) -> Option<TimelockProposal> {
        env.storage().persistent().get(&DataKey::Timelock(proposal_id))
    }

    /// Withdraw a vouch before any loan is active, returning the exact stake to the voucher.
    ///
    /// Fails with `ContractError::PoolBorrowerActiveLoan` if the borrower currently
    /// has an active (non-repaid, non-defaulted) loan. Repaid or defaulted loan
    /// records in storage do not block withdrawal.
    pub fn withdraw_vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        voucher.require_auth();
        Self::require_not_paused(&env)?;

        // Block only while a loan is genuinely active; repaid/defaulted records
        // in storage must not permanently lock voucher funds (fixes issue #14).
        if Self::has_active_loan(&env, &borrower) {
            return Err(ContractError::PoolBorrowerActiveLoan);
        }

        let mut vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .ok_or(ContractError::NoActiveLoan)?; // reuse: "no vouch found"

        let idx = vouches
            .iter()
            .position(|v| v.voucher == voucher)
            .ok_or(ContractError::UnauthorizedCaller)? as u32;

        let stake = vouches.get(idx).unwrap().stake;
        vouches.remove(idx);

        if vouches.is_empty() {
            env.storage()
                .persistent()
                .remove(&DataKey::Vouches(borrower.clone()));
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::Vouches(borrower.clone()), &vouches);
        }

        Self::token(&env).transfer(&env.current_contract_address(), &voucher, &stake);

        env.events().publish(
            (symbol_short!("vouch"), symbol_short!("withdrawn")),
            (voucher, borrower, stake),
        );

        Ok(())
    }

    /// Transfer ownership of a stake position for a borrower from one address to another.
    pub fn transfer_vouch(
        env: Env,
        from: Address,
        to: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        from.require_auth();
        Self::require_not_paused(&env)?;

        if from == to {
            return Ok(());
        }

        // Only allow transfer before a loan is active (consistent with withdraw_vouch).
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

        let from_idx = vouches
            .iter()
            .position(|v| v.voucher == from)
            .expect("from voucher not found") as u32;

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

        let dc: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::DefaultCount(borrower.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::DefaultCount(borrower.clone()), &(dc + 1));

        let mut total_slash: i128 = 0;
        for v in vouches.iter() {
            total_slash += v.stake * cfg.slash_bps / 10_000;
        }
        let treasury: i128 = env
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


    // ── Loan Deadline ─────────────────────────────────────────────────────────

        QuorumCreditContractClient::new(env, &contract_id)
            .initialize(&admin, &admin, &token_id.address());
    /// Callable by anyone after the loan deadline has passed.
    /// Applies the standard slash penalty.

    pub fn auto_slash(env: Env, borrower: Address) {
        // ── CHECKS ────────────────────────────────────────────────────────────
        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(borrower.clone()))
            .expect("no active loan");


        assert!(!loan.repaid, "loan already repaid");
        assert!(!loan.defaulted, "loan already defaulted");

        let cfg = Self::config(&env);
        // saturating_add prevents u64 overflow on pathological deadline/grace_period values.
        let slash_threshold = loan.deadline.saturating_add(cfg.grace_period);
        assert!(
            env.ledger().timestamp() > slash_threshold,
            "loan grace period has not passed"
        );

        let token = Self::token(&env);
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
        Self::add_slash_balance(&env, total_slash);

        // ── INTERACTIONS ──────────────────────────────────────────────────────
        let token = Self::token(&env);
        for v in vouches.iter() {
            let slash_amount = v.stake * cfg.slash_bps / 10_000;
            let returned = v.stake - slash_amount;
            if returned > 0 {
                token.transfer(&env.current_contract_address(), &v.voucher, &returned);
            }
        }

        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftExternalClient::new(&env, &nft_addr).burn(&borrower);
        }
    }

    // ── Loan Extension ────────────────────────────────────────────────────────

    /// A voucher signals consent for extending the loan deadline of a borrower
    /// they have staked on. Consent is recorded but does not by itself extend
    /// the loan — an admin must call `extend_loan` to finalise the extension.
    pub fn consent_extension(env: Env, voucher: Address, borrower: Address) {
        voucher.require_auth();
        Self::require_not_paused(&env).expect("contract is paused");

        // Voucher must have an active stake on this borrower.
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .expect("no vouches found for borrower");
        assert!(
            vouches.iter().any(|v| v.voucher == voucher),
            "caller is not a voucher for this borrower"
        );

        let mut consents: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::ExtensionConsents(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        assert!(
            !consents.iter().any(|a| a == voucher),
            "already consented"
        );
        consents.push_back(voucher.clone());
        env.storage()
            .persistent()
            .set(&DataKey::ExtensionConsents(borrower.clone()), &consents);

        env.events().publish(
            (symbol_short!("ext"), symbol_short!("consent")),
            (voucher, borrower),
        );
    }

    /// Admin extends the loan deadline for a borrower.
    ///
    /// `new_deadline` must be strictly greater than the current deadline.
    /// The admin may extend unilaterally; voucher consents recorded via
    /// `consent_extension` are cleared after a successful extension so the
    /// slate is clean for any future extension request.
    pub fn extend_loan(
        env: Env,
        admin_signers: Vec<Address>,
        borrower: Address,
        new_deadline: u64,
    ) -> Result<(), ContractError> {
        Self::require_admin_approval(&env, &admin_signers);
        Self::require_not_paused(&env)?;

    /// Admin permanently bans a borrower from requesting future loans.
    pub fn blacklist(env: Env, admin_signers: Vec<Address>, borrower: Address) {
        Self::require_admin_approval(&env, &admin_signers);
        env.storage()
            .persistent()
            .set(&DataKey::Blacklisted(borrower), &true);
    }

    /// Returns true if the borrower has been permanently blacklisted.
    pub fn is_blacklisted(env: Env, borrower: Address) -> bool {
        env.storage()
            .persistent()
            .get::<DataKey, bool>(&DataKey::Blacklisted(borrower))
            .unwrap_or(false)
    }

    /// Admin sets the minimum stake amount required per vouch (in stroops).
    pub fn set_min_stake(env: Env, admin_signers: Vec<Address>, amount: i128) {
        Self::require_admin_approval(&env, &admin_signers);
        assert!(amount >= 0, "min stake cannot be negative");
        env.storage().instance().set(&DataKey::MinStake, &amount);
        env.events().publish(
            (symbol_short!("admin"), symbol_short!("minstake")),
            (admin_signers.get(0).unwrap(), amount, env.ledger().timestamp()),
        );
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
        env.events().publish(
            (symbol_short!("admin"), symbol_short!("maxloan")),
            (admin_signers.get(0).unwrap(), amount, env.ledger().timestamp()),
        );
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
        env.events().publish(
            (symbol_short!("admin"), symbol_short!("minvchrs")),
            (admin_signers.get(0).unwrap(), count, env.ledger().timestamp()),
        );
    }

    /// Returns the current minimum voucher count (0 means no minimum).
    pub fn get_min_vouchers(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MinVouchers)
            .unwrap_or(0)
    }

    /// Admin sets the maximum loan-to-stake ratio (as a percentage, e.g. 150 = 150%).
    /// A value of 0 is rejected; use set_config to disable the ratio check entirely
    /// by setting max_loan_to_stake_ratio to a very large number.
    pub fn set_max_loan_to_stake_ratio(
        env: Env,
        admin_signers: Vec<Address>,
        ratio: u32,
    ) {
        Self::require_admin_approval(&env, &admin_signers);
        assert!(ratio > 0, "max_loan_to_stake_ratio must be greater than zero");
        let mut cfg = Self::config(&env);
        cfg.max_loan_to_stake_ratio = ratio;
        env.storage().instance().set(&DataKey::Config, &cfg);
    }

    /// Returns the current maximum loan-to-stake ratio (percentage).
    pub fn get_max_loan_to_stake_ratio(env: Env) -> u32 {
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
        // grace_period of 0 is valid — means no grace period, slash allowed immediately after deadline.
        env.storage().instance().set(&DataKey::Config, &config);
        env.events().publish(
            (symbol_short!("admin"), symbol_short!("config")),
            (admin_signers.get(0).unwrap(), env.ledger().timestamp()),
        );
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
        env.events().publish(
            (symbol_short!("admin"), symbol_short!("repnft")),
            (admin_signers.get(0).unwrap(), nft_contract, env.ledger().timestamp()),
        );
    }

        pub fn remove_voucher(
        env: Env,
        admin_signers: Vec<Address>,
        voucher: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {

        Self::require_admin_approval(&env, &admin_signers);
        Self::require_not_paused(&env)?;
 
        let mut vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .ok_or(ContractError::NoVouchesForBorrower)?;
 
        let idx = vouches
            .iter()
            .position(|v| v.voucher == voucher)
            .ok_or(ContractError::VoucherNotFound)? as u32;
 

        let stake = vouches.get(idx).unwrap().stake;
        vouches.remove(idx);
 
        if vouches.is_empty() {
            env.storage()
                .persistent()
                .remove(&DataKey::Vouches(borrower.clone()));
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::Vouches(borrower.clone()), &vouches);
        }
 

        Self::token(&env).transfer(
            &env.current_contract_address(),
            &voucher,
            &stake,
        );
 
        env.events().publish(
            (symbol_short!("vouch"), symbol_short!("removed")),
            (voucher, borrower, stake),
        );
 
        Ok(())
    }

    // ── Admin: Key Management (M-of-N Multisig) ───────────────────────────────
    //
    // All mutations to the admin set require the current quorum to sign,
    // preventing a single compromised key from unilaterally modifying governance.
    //
    // Key management recommendations:
    //   - Use hardware wallets (Ledger/Trezor) for each admin key.
    //   - Store key backups in geographically separate, offline locations.
    //   - Set admin_threshold to at least ceil(N/2)+1 for N admins (e.g. 3-of-5).
    //   - Rotate keys periodically using rotate_admin; never reuse compromised keys.
    //   - After any key rotation, verify the new admin set with get_admins().
    //   - Never share private keys; each admin should control exactly one key.

    /// Add a new admin to the set. Requires current quorum approval.
    /// The new admin is appended; threshold is unchanged.
    pub fn add_admin(env: Env, admin_signers: Vec<Address>, new_admin: Address) {
        Self::require_admin_approval(&env, &admin_signers);

        let mut cfg = Self::config(&env);

        assert!(
            !cfg.admins.iter().any(|a| a == new_admin),
            "address is already an admin"
        );

        cfg.admins.push_back(new_admin.clone());
        env.storage().instance().set(&DataKey::Config, &cfg);

        env.events().publish(
            (symbol_short!("admin"), symbol_short!("added")),
            new_admin,
        );
    }

    /// Remove an existing admin from the set. Requires current quorum approval.
    /// The threshold must remain satisfiable after removal (threshold <= remaining admins).
    pub fn remove_admin(env: Env, admin_signers: Vec<Address>, admin_to_remove: Address) {
        Self::require_admin_approval(&env, &admin_signers);

        let mut cfg = Self::config(&env);

        let idx = cfg
            .admins
            .iter()
            .position(|a| a == admin_to_remove)
            .expect("address is not an admin") as u32;

        cfg.admins.remove(idx);

        assert!(
            !cfg.admins.is_empty(),
            "cannot remove the last admin"
        );
        assert!(
            cfg.admin_threshold <= cfg.admins.len(),
            "removal would make threshold unsatisfiable"
        );

        env.storage().instance().set(&DataKey::Config, &cfg);

        env.events().publish(
            (symbol_short!("admin"), symbol_short!("removed")),
            admin_to_remove,
        );
    }

    /// Atomically replace one admin key with another. Requires current quorum approval.
    /// Use this for key rotation — the threshold is preserved and the admin count stays the same.
    pub fn rotate_admin(
        env: Env,
        admin_signers: Vec<Address>,
        old_admin: Address,
        new_admin: Address,
    ) {
        Self::require_admin_approval(&env, &admin_signers);

        assert!(old_admin != new_admin, "old and new admin must differ");

        let mut cfg = Self::config(&env);

        assert!(
            !cfg.admins.iter().any(|a| a == new_admin),
            "new admin is already in the admin set"
        );

        let idx = cfg
            .admins
            .iter()
            .position(|a| a == old_admin)
            .expect("old admin not found") as u32;

        cfg.admins.set(idx, new_admin.clone());
        env.storage().instance().set(&DataKey::Config, &cfg);

        env.events().publish(
            (symbol_short!("admin"), symbol_short!("rotated")),
            (old_admin, new_admin),
        );
    }

    /// Update the quorum threshold. Requires current quorum approval.
    /// New threshold must be > 0 and <= current admin count.
    pub fn set_admin_threshold(env: Env, admin_signers: Vec<Address>, new_threshold: u32) {
        Self::require_admin_approval(&env, &admin_signers);

        let mut cfg = Self::config(&env);

        assert!(new_threshold > 0, "threshold must be greater than zero");
        assert!(
            new_threshold <= cfg.admins.len(),
            "threshold cannot exceed admin count"
        );

        cfg.admin_threshold = new_threshold;
        env.storage().instance().set(&DataKey::Config, &cfg);

        env.events().publish(
            (symbol_short!("admin"), symbol_short!("thresh")),
            new_threshold,
        );
    }

    // ── Admin: Protocol Fee ───────────────────────────────────────────────────

    /// Admin sets the protocol fee applied to interactions (in basis points).
    pub fn set_protocol_fee(env: Env, admin_signers: Vec<Address>, fee_bps: u32) {
        Self::require_admin_approval(&env, &admin_signers);
        assert!(fee_bps <= 10_000, "fee_bps must not exceed 10000");
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &fee_bps);
        env.events().publish(
            (symbol_short!("admin"), symbol_short!("fee")),
            (admin_signers.get(0).unwrap(), fee_bps, env.ledger().timestamp()),
        );
    }

    /// Returns the current protocol fee (0 if not set).
    pub fn get_protocol_fee(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0)
    }

    /// Admin sets the treasury address that receives protocol fees on repayment.
    pub fn set_fee_treasury(env: Env, admin_signers: Vec<Address>, treasury: Address) {
        Self::require_admin_approval(&env, &admin_signers);
        env.storage()
            .instance()
            .set(&DataKey::FeeTreasury, &treasury);
    }

    /// Returns the current fee treasury address, if set.
    pub fn get_fee_treasury(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::FeeTreasury)
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
        env.events().publish(
            (symbol_short!("admin"), symbol_short!("pause")),
            (admin_signers.get(0).unwrap(), env.ledger().timestamp()),
        );
    }

    /// Unpause the contract.
    pub fn unpause(env: Env, admin_signers: Vec<Address>) {
        Self::require_admin_approval(&env, &admin_signers);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish(
            (symbol_short!("admin"), symbol_short!("unpause")),
            (admin_signers.get(0).unwrap(), env.ledger().timestamp()),
        );
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

    /// Admin-only paginated view of all loan records.
    /// Returns the slice of LoanRecords for the given page (0-indexed).
    pub fn get_all_loans(env: Env, page: u32, page_size: u32) -> Vec<LoanRecord> {
        let config = Self::config(&env);
        assert!(config.admins.contains(&env.invoker()), "unauthorized");

        assert!(page_size > 0, "page_size must be greater than zero");

        let borrowers: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::BorrowerList)
            .unwrap_or(Vec::new(&env));

        let start = (page * page_size) as usize;
        let mut result = Vec::new(&env);

        for i in start..(start + page_size as usize).min(borrowers.len() as usize) {
            let borrower = borrowers.get(i as u32).unwrap();
            if let Some(loan) = env.storage().persistent().get(&DataKey::Loan(borrower)) {
                result.push_back(loan);
            }
        }

        result
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

    /// Returns the total number of loans ever disbursed to a borrower (including active, repaid, and defaulted).
    pub fn loan_count(env: Env, borrower: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::LoanCount(borrower))
            .unwrap_or(0)
    }

    /// Returns the total number of defaults for a borrower (slash, auto_slash, or claim_expired_loan).
    pub fn default_count(env: Env, borrower: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::DefaultCount(borrower))
            .unwrap_or(0)
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
                    total_yield: amount * cfg.yield_bps / 10_000,
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

    // ── Private Helpers ───────────────────────────────────────────────────────

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

    /// Returns `Err(InsufficientFunds)` if `amount` is not strictly positive (≤ 0).
    /// Use this for all numeric inputs that must be > 0 (stakes, loan amounts, thresholds).
    fn require_positive_amount(_env: &Env, amount: i128) -> Result<(), ContractError> {
        if amount <= 0 {
            return Err(ContractError::InsufficientFunds);
        }
        Ok(())
    }

    fn config(env: &Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .expect("not initialized")
    }

    fn add_slash_balance(env: &Env, amount: i128) {
        let current: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SlashTreasury)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::SlashTreasury, &(current + amount));
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

    fn token_client(env: &Env) -> token::Client<'_> {
        Self::token(env)
    }

    /// Core slash logic shared by `slash` (direct) and `execute_action` (timelocked).
    fn do_slash(env: &Env, borrower: Address) {
        let mut loan: LoanRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Loan(borrower.clone()))
            .expect("no active loan");

        if loan.repaid || loan.defaulted {
            panic_with_error!(env, ContractError::NoActiveLoan);
        }

        let cfg = Self::config(env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(env));

        loan.defaulted = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower.clone()), &loan);
        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower.clone()));

        let token = Self::token(env);
        let mut total_slashed: i128 = 0;
        for v in vouches.iter() {
            let slash_amount = v.stake * cfg.slash_bps / 10_000;
            let returned = v.stake - slash_amount;
            if returned > 0 {
                token.transfer(&env.current_contract_address(), &v.voucher, &returned);
            }
            total_slashed += slash_amount;
        }

        let treasury: i128 = env
            .storage()
            .instance()
            .get(&DataKey::SlashTreasury)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::SlashTreasury, &(treasury + total_slashed));

        if let Some(nft_addr) = env
            .storage()
            .instance()
            .get::<DataKey, Address>(&DataKey::ReputationNft)
        {
            ReputationNftExternalClient::new(env, &nft_addr).burn(&borrower);
        }

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("slashed")),
            (borrower, loan.amount, total_slashed),
        );
    }

    fn require_admin_approval(env: &Env, admin_signers: &Vec<Address>) {
        let config = Self::config(env);
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

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Address, Env, Vec,
    };
    use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};

    fn single_admin_signers(env: &Env, admin: &Address) -> Vec<Address> {
        let mut v = Vec::new(env);
        v.push_back(admin.clone());
        v
    }

    fn address_vec(env: &Env, addrs: &[Address]) -> Vec<Address> {
        let mut v = Vec::new(env);
        for a in addrs {
            v.push_back(a.clone());
        }
        v
    }

    /// Advance the ledger clock past the minimum vouch age so vouches become usable.
    fn advance_past_vouch_age(env: &Env) {
        let current = env.ledger().timestamp();
        env.ledger().set_timestamp(current + MIN_VOUCH_AGE);
    }

    fn setup(env: &Env) -> (Address, Address, Address, Address, Address) {
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000_000);

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
    ) -> (Address, Address, Address, Address, Address, Address, Address) {
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

        (contract_id, token_id.address(), admin_one, admin_two, admin_three, borrower, voucher)
    }

    fn setup_with_reputation(env: &Env) -> (Address, Address, Address, Address, Address, Address) {
        let (contract_id, token_addr, admin, borrower, voucher) = setup(env);
        let client = QuorumCreditContractClient::new(env, &contract_id);
        let admin_signers = single_admin_signers(env, &admin);

        let nft_id = env.register_contract(None, reputation::ReputationNftContract);
        reputation::ReputationNftContractClient::new(env, &nft_id).initialize(&contract_id);
        client.set_reputation_nft(&admin_signers, &nft_id);

        (contract_id, token_addr, admin, borrower, voucher, nft_id)
    }

    // ── Core Tests ────────────────────────────────────────────────────────────

    #[test]
    fn test_vouch_and_loan_disbursed() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        let loan = client.get_loan(&borrower).unwrap();
        assert_eq!(loan.amount, 500_000);
        assert!(!loan.repaid);
        assert!(!loan.defaulted);
        assert!(loan.created_at > 0);
        assert!(loan.deadline > loan.created_at);
    }

    #[test]
    #[should_panic(expected = "voucher cannot vouch for self")]
    fn test_vouch_self_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&borrower, &borrower, &1_000_000);
    }

    /// Issue 4: a zero-stake vouch must be rejected to prevent inflating the
    /// vouch count without contributing to the loan threshold.
    #[test]
    #[should_panic(expected = "stake must be greater than zero")]
    fn test_vouch_zero_stake_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &0);
    }

    #[test]
    fn test_repay_gives_voucher_yield() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);

        assert_eq!(token.balance(&voucher), 10_010_000);
    }

    #[test]
    fn test_slash_burns_half_stake() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower);

        assert_eq!(token.balance(&voucher), 9_500_000);
        assert!(client.get_loan(&borrower).unwrap().defaulted);
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
    fn test_repay_nonexistent_loan_should_fail() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let result = client.try_repay(&borrower, &100_000);
        assert_eq!(result, Err(Ok(ContractError::NoActiveLoan)));
    }

    #[test]
    fn test_repay_mismatched_borrower_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let attacker = Address::generate(&env);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        token_admin.mint(&attacker, &10_000_000);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);

        let result = client.try_repay(&attacker, &500_000);
        assert_eq!(result, Err(Ok(ContractError::NoActiveLoan)));

        client.repay(&borrower, &500_000);
    }

    // ── Min Yield Stake Tests ─────────────────────────────────────────────────

    #[test]
    #[should_panic(expected = "stake too small: would produce zero yield due to integer truncation")]
    fn test_vouch_small_stake_below_min_yield_stake_rejected() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &49);
    }

    #[test]
    fn test_vouch_at_min_yield_stake_earns_nonzero_yield() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &100_000, &1_000_000);
        client.repay(&borrower, &100_000);

        let initial_balance: i128 = 10_000_000;
        let final_balance = token.balance(&voucher);
        assert!(
            final_balance > initial_balance,
            "voucher yield was zero for min_yield_stake; got balance {}",
            final_balance
        );
    }

    // ── Partial Repayment Tests ───────────────────────────────────────────────

    #[test]
    fn test_partial_repay_updates_amount_repaid() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &600_000, &1_000_000);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &600_000, &1_000_000);

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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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
        assert_eq!(event_slashed, 500_000);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &0);
    }

    // ── Stake Management Tests ────────────────────────────────────────────────

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

        advance_past_vouch_age(&env);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        client.decrease_stake(&voucher, &borrower, &100_000);
    }

    // ── Loan Request Tests ────────────────────────────────────────────────────

    #[test]
    #[should_panic(expected = "loan amount must meet minimum threshold")]
    fn test_zero_amount_loan_should_fail() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &0, &1_000_000);
    }

    #[test]
    #[should_panic(expected = "borrower already has an active loan")]
    fn test_request_loan_rejects_overwrite_of_active_loan() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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
            &admin, &admins, &1, &token_id.address(),
        );

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &1_500_000, &1_000_000);
        client.repay(&borrower, &1_500_000);

        let result = client.try_request_loan(&borrower, &Vec::new(&env), &2_000_000, &1_000_000);
        assert!(result.is_err());

        advance_past_vouch_age(&env);
        let result = client.try_request_loan(&borrower, &Vec::new(&env), &1_500_000, &1_000_000);
        assert_eq!(result, Err(Ok(ContractError::InsufficientFunds)));
    }

    #[test]
    fn test_over_collateralization_check() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        // 150% of 1_000_000 = 1_500_000 max
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &1_500_000, &1_000_000);
        client.repay(&borrower, &1_500_000);

        let result = client.try_request_loan(&borrower, &Vec::new(&env), &2_000_000, &1_000_000);
        assert!(result.is_err());
    }

    #[test]
    #[should_panic(expected = "maximum vouchers per loan exceeded")]
    fn test_vouch_exceeds_max_limit() {
        let env = Env::default();
        env.budget().reset_unlimited();
        let (contract_id, token_addr, _admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        for _ in 0..DEFAULT_MAX_VOUCHERS {
            let v = Address::generate(&env);
            token_admin.mint(&v, &10_000_000);
            client.vouch(&v, &borrower, &1_000_000);
        }

        let extra = Address::generate(&env);
        token_admin.mint(&extra, &10_000_000);
        client.vouch(&extra, &borrower, &1_000_000);
    }

    #[test]
    fn test_repay_with_max_vouchers() {
        let env = Env::default();
        env.budget().reset_unlimited();
        let (contract_id, token_addr, _admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        for _ in 0..DEFAULT_MAX_VOUCHERS {
            let v = Address::generate(&env);
            token_admin.mint(&v, &10_000_000);
            client.vouch(&v, &borrower, &1_000_000);
        }

        advance_past_vouch_age(&env);
        client.request_loan(
            &borrower,
            &Vec::new(&env),
            &500_000,
            &(DEFAULT_MAX_VOUCHERS as i128 * 1_000_000),
        );
        client.repay(&borrower, &500_000);

        assert!(client.get_loan(&borrower).unwrap().repaid);
    }

    #[test]
    fn test_max_vouchers_configurable_via_set_config() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        // Lower the cap to 2
        let mut cfg = client.get_config();
        cfg.max_vouchers = 2;
        client.set_config(&admin_signers, &cfg);
        assert_eq!(client.get_config().max_vouchers, 2);

        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.vouch(&voucher2, &borrower, &1_000_000);

        // Third vouch must be rejected
        let voucher3 = Address::generate(&env);
        token_admin.mint(&voucher3, &10_000_000);
        let result = client.try_vouch(&voucher3, &borrower, &1_000_000);
        assert!(result.is_err());
    }

    #[test]
    fn test_slash_with_max_vouchers() {
        let env = Env::default();
        env.budget().reset_unlimited();
        let (contract_id, token_addr, admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut vouchers = soroban_sdk::Vec::new(&env);
        for _ in 0..DEFAULT_MAX_VOUCHERS {
            let v = Address::generate(&env);
            token_admin.mint(&v, &10_000_000);
            client.vouch(&v, &borrower, &1_000_000);
            vouchers.push_back(v);
        }

        client.request_loan(
            &borrower,
            &Vec::new(&env),
            &500_000,
            &(DEFAULT_MAX_VOUCHERS as i128 * 1_000_000),
        );
        client.slash(&admin_signers, &borrower);

        assert!(client.get_loan(&borrower).unwrap().defaulted);
        // Each voucher had 1_000_000 staked, 50% slashed → 500_000 returned
        for v in vouchers.iter() {
            assert_eq!(TokenClient::new(&env, &token_addr).balance(&v), 9_500_000);
        }
    }

    // ── View Tests ────────────────────────────────────────────────────────────

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
    fn test_get_contract_balance() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_contract_balance(), 50_000_000);
        client.vouch(&voucher, &borrower, &1_000_000);
        assert_eq!(client.get_contract_balance(), 51_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower);

        assert_eq!(client.get_slash_treasury(), 500_000);

        let treasury_recipient = Address::generate(&env);
        client.slash_treasury(&admin_signers, &treasury_recipient);

        assert_eq!(token.balance(&treasury_recipient), 500_000);
        assert_eq!(client.get_slash_treasury(), 0);
        assert_eq!(client.get_contract_balance(), 50_500_000);
    }

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
        assert_eq!(client.voucher_history(&unknown).len(), 0);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(client.loan_status(&borrower), LoanStatus::Active);
    }

    #[test]
    fn test_loan_status_repaid() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower);
        assert_eq!(client.loan_status(&borrower), LoanStatus::Defaulted);
    }

    // ── Slash Treasury Tests ──────────────────────────────────────────────────

    #[test]
    fn test_slash_treasury_withdrawal() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.pause(&admin_signers);

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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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
        assert_eq!(client.get_vouches(&borrower).unwrap().len(), 1);
    }

    // ── Loan Deadline / Auto-Slash Tests ──────────────────────────────────────

    #[test]
    fn test_deadline_set_from_loan_duration() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.loan_duration = 1_000;
        client.set_config(&admin_signers, &cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        let disbursement_ts = env.ledger().timestamp();
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        let loan = client.get_loan(&borrower).unwrap();
        assert_eq!(loan.deadline, disbursement_ts + 1_000);
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
        advance_past_vouch_age(&env);
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
        advance_past_vouch_age(&env);
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
        advance_past_vouch_age(&env);
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
    fn test_loan_records_disbursement_timestamp() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        let disbursement_ts = env.ledger().timestamp();
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        let loan = client.get_loan(&borrower).unwrap();
        assert_eq!(loan.disbursement_timestamp, disbursement_ts);
        assert_eq!(loan.created_at, disbursement_ts);
        assert!(loan.deadline > disbursement_ts);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        assert!(!client.is_eligible(&borrower, &1_000_000));
    }

    #[test]
    fn test_is_eligible_returns_true_after_loan_repaid() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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
        assert_eq!(QuorumCreditContractClient::new(&env, &contract_id).get_min_stake(), 0);
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
        assert_eq!(client.get_vouches(&borrower).unwrap().len(), 1);
    }

    // ── Batch Vouch Tests ─────────────────────────────────────────────────────

    #[test]
    fn test_batch_vouch_vouches_multiple_borrowers() {
        let env = Env::default();
        let (contract_id, token_addr, _admin, _borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);

        let borrower_a = Address::generate(&env);
        let borrower_b = Address::generate(&env);
        let borrower_c = Address::generate(&env);

        let mut borrowers = Vec::new(&env);
        borrowers.push_back(borrower_a.clone());
        borrowers.push_back(borrower_b.clone());
        borrowers.push_back(borrower_c.clone());

        let mut stakes = Vec::new(&env);
        stakes.push_back(1_000_000i128);
        stakes.push_back(500_000i128);
        stakes.push_back(200_000i128);

        client.batch_vouch(&voucher, &borrowers, &stakes);

        assert_eq!(client.get_vouches(&borrower_a).unwrap().get(0).unwrap().stake, 1_000_000);
        assert_eq!(client.get_vouches(&borrower_b).unwrap().get(0).unwrap().stake, 500_000);
        assert_eq!(client.get_vouches(&borrower_c).unwrap().get(0).unwrap().stake, 200_000);
        // 10_000_000 - 1_000_000 - 500_000 - 200_000 = 8_300_000
        assert_eq!(token.balance(&voucher), 8_300_000);
    }

    #[test]
    #[should_panic(expected = "borrowers and stakes length mismatch")]
    fn test_batch_vouch_length_mismatch_rejected() {
        let env = Env::default();
        let (contract_id, _, _, _, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let mut borrowers = Vec::new(&env);
        borrowers.push_back(Address::generate(&env));
        let stakes: Vec<i128> = Vec::new(&env);

        client.batch_vouch(&voucher, &borrowers, &stakes);
    }

    #[test]
    #[should_panic(expected = "batch cannot be empty")]
    fn test_batch_vouch_empty_rejected() {
        let env = Env::default();
        let (contract_id, _, _, _, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.batch_vouch(&voucher, &Vec::new(&env), &Vec::new(&env));
    }

    #[test]
    fn test_batch_vouch_duplicate_aborts_batch() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // Pre-vouch so the second batch entry is a duplicate.
        client.vouch(&voucher, &borrower, &1_000_000);

        let mut borrowers = Vec::new(&env);
        borrowers.push_back(Address::generate(&env));
        borrowers.push_back(borrower.clone());

        let mut stakes = Vec::new(&env);
        stakes.push_back(1_000_000i128);
        stakes.push_back(500_000i128);

        let result = client.try_batch_vouch(&voucher, &borrowers, &stakes);
        assert_eq!(result, Err(Ok(ContractError::DuplicateVouch)));
    }

    // ── Max Loan Amount Tests ─────────────────────────────────────────────────

    #[test]
    fn test_get_max_loan_amount_defaults_to_zero() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        assert_eq!(QuorumCreditContractClient::new(&env, &contract_id).get_max_loan_amount(), 0);
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
        let result = client.try_request_loan(&borrower, &Vec::new(&env), &600_000, &1_000_000);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(client.get_loan(&borrower).unwrap().amount, 500_000);
    }

    // ── Min Vouchers Tests ────────────────────────────────────────────────────

    #[test]
    fn test_get_min_vouchers_defaults_to_zero() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        assert_eq!(QuorumCreditContractClient::new(&env, &contract_id).get_min_vouchers(), 0);
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

        let result = client.try_request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(result, Err(Ok(ContractError::InsufficientVouchers)));

        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);
        client.vouch(&voucher2, &borrower, &1_000_000);
        let result = client.try_request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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

        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(client.get_loan(&borrower).unwrap().amount, 500_000);
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
        assert_eq!(cfg.vouch_cooldown_secs, DEFAULT_VOUCH_COOLDOWN_SECS);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
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


    // ── Grace period tests ────────────────────────────────────────────────────

    /// Helper: set a short loan_duration + grace_period, vouch, and request a loan.
    fn setup_grace_period_loan(
        env: &Env,
        loan_duration: u64,
        grace_period: u64,
    ) -> (Address, Address, Address, Address) {
        let (contract_id, token_addr, _admin, borrower, voucher) = setup(env);
        let client = QuorumCreditContractClient::new(env, &contract_id);

        let mut cfg = client.get_config();
        cfg.loan_duration = loan_duration;
        cfg.grace_period = grace_period;
        client.set_config(&cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &500_000, &1_000_000);

        (contract_id, token_addr, borrower, voucher)
    }

    #[test]
    #[should_panic(expected = "loan grace period has not passed")]
    fn test_auto_slash_blocked_during_grace_period() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, borrower, _voucher) =
            setup_grace_period_loan(&env, 1_000, 500);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // deadline = 1_001_000; slash_threshold = 1_001_500
        // advance to 1_001_200 — past deadline but still inside grace period
        env.ledger().set_timestamp(1_001_200);
        client.auto_slash(&borrower);
    }

    #[test]
    fn test_auto_slash_allowed_after_grace_period() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, token_addr, borrower, voucher) =
            setup_grace_period_loan(&env, 1_000, 500);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = soroban_sdk::token::Client::new(&env, &token_addr);

        // deadline = 1_001_000; slash_threshold = 1_001_500
        // advance to 1_001_501 — one second past the grace period
        env.ledger().set_timestamp(1_001_501);
        client.auto_slash(&borrower);

        assert!(client.get_loan(&borrower).unwrap().defaulted);
        assert_eq!(token.balance(&voucher), 9_500_000);
    }

    #[test]
    #[should_panic(expected = "loan grace period has not passed")]
    fn test_auto_slash_blocked_exactly_at_slash_threshold() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, borrower, _voucher) =
            setup_grace_period_loan(&env, 1_000, 500);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // deadline = 1_001_000; slash_threshold = 1_001_500
        // timestamp == slash_threshold: condition is `>`, so this must be rejected
        env.ledger().set_timestamp(1_001_500);
        client.auto_slash(&borrower);
    }

    #[test]
    fn test_auto_slash_allowed_one_second_past_threshold() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, borrower, _voucher) =
            setup_grace_period_loan(&env, 1_000, 500);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // deadline = 1_001_000; slash_threshold = 1_001_500
        env.ledger().set_timestamp(1_001_501);
        client.auto_slash(&borrower);

        assert!(client.get_loan(&borrower).unwrap().defaulted);
    }

    #[test]
    fn test_auto_slash_zero_grace_period_behaves_like_original() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, token_addr, borrower, voucher) =
            setup_grace_period_loan(&env, 1_000, 0);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = soroban_sdk::token::Client::new(&env, &token_addr);

        // With grace_period = 0, slash_threshold == deadline; any timestamp > deadline works.
        env.ledger().set_timestamp(1_001_001);
        client.auto_slash(&borrower);

        assert!(client.get_loan(&borrower).unwrap().defaulted);
        assert_eq!(token.balance(&voucher), 9_500_000);
    }

    #[test]
    #[should_panic(expected = "loan already repaid")]
    fn test_auto_slash_blocked_on_repaid_loan() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _token_addr, borrower, _voucher) =
            setup_grace_period_loan(&env, 1_000, 500);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // Repay before deadline.
        client.repay(&borrower);

        // Attempt auto_slash after grace period — must be rejected.
        env.ledger().set_timestamp(1_002_000);
        client.auto_slash(&borrower);
    }

    #[test]
    fn test_default_grace_period_is_three_days() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_config().grace_period, DEFAULT_GRACE_PERIOD);
        assert_eq!(DEFAULT_GRACE_PERIOD, 3 * 24 * 60 * 60);
    }

    #[test]
    fn test_set_config_updates_grace_period() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let mut cfg = client.get_config();
        cfg.grace_period = 7 * 24 * 60 * 60; // 7 days
        client.set_config(&cfg);

        assert_eq!(client.get_config().grace_period, 7 * 24 * 60 * 60);

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

    #[test]
    fn test_protocol_fee_deducted_on_repayment_and_sent_to_treasury() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let treasury = Address::generate(&env);
        // 100 bps = 1% fee
        client.set_protocol_fee(&admin_signers, &100);
        client.set_fee_treasury(&admin_signers, &treasury);

        client.vouch(&voucher, &borrower, &1_000_000);
        // loan = 500_000; fee = 500_000 * 100 / 10_000 = 5_000
        // distributable = 495_000; yield = 495_000 * 200 / 10_000 = 9_900
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);

        // treasury receives the protocol fee
        assert_eq!(token.balance(&treasury), 5_000);
        // voucher gets stake back + yield on distributable amount
        // 1_000_000 + 9_900 = 1_009_900; started with 10_000_000 - 1_000_000 = 9_000_000
        assert_eq!(token.balance(&voucher), 10_009_900);
    }

    #[test]
    fn test_protocol_fee_zero_no_treasury_transfer() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token = TokenClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let treasury = Address::generate(&env);
        // fee stays at 0 (default), treasury set but should receive nothing
        client.set_fee_treasury(&admin_signers, &treasury);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);

        assert_eq!(token.balance(&treasury), 0);
        // yield unchanged from no-fee case: 500_000 * 200 / 10_000 = 10_000
        assert_eq!(token.balance(&voucher), 10_010_000);
    }

    #[test]
    fn test_set_and_get_fee_treasury() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        assert!(client.get_fee_treasury().is_none());
        let treasury = Address::generate(&env);
        client.set_fee_treasury(&admin_signers, &treasury);
        assert_eq!(client.get_fee_treasury().unwrap(), treasury);
    }

    // ── Admin Tests ───────────────────────────────────────────────────────────

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
    fn test_multisig_threshold_allows_admin_operation() {
        let env = Env::default();
        let (contract_id, _token_addr, admin_one, admin_two, _admin_three, _borrower, _voucher) =
            setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one.clone(), admin_two.clone()]);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        client.pause(&signers);

        assert!(client.get_paused());
        assert_eq!(client.get_admin_threshold(), 2);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        assert_eq!(client.repayment_count(&borrower), 1);

        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        assert_eq!(client.repayment_count(&borrower), 2);

        let borrower2 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);

        assert_eq!(client.repayment_count(&borrower2), 0);
        client.vouch(&voucher2, &borrower2, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower2, &Vec::new(&env), &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower2);
        assert_eq!(client.repayment_count(&borrower2), 0);
    }

    // ── Loan Count Tests ──────────────────────────────────────────────────────

    #[test]
    fn test_loan_count_zero_for_new_borrower() {
        let env = Env::default();
        let (contract_id, _, _, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        assert_eq!(client.loan_count(&borrower), 0);
    }

    #[test]
    fn test_loan_count_increments_on_each_disbursement() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);

        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(client.loan_count(&borrower), 1);

        client.repay(&borrower, &500_000);

        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(client.loan_count(&borrower), 2);

        client.repay(&borrower, &500_000);
        assert_eq!(client.loan_count(&borrower), 2); // repay doesn't change loan_count
    }

    #[test]
    fn test_loan_count_includes_defaulted_loans() {
        let env = Env::default();
        let (contract_id, _, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(client.loan_count(&borrower), 1);

        client.slash(&admin_signers, &borrower);
        assert_eq!(client.loan_count(&borrower), 1); // slash doesn't change loan_count
    }

    #[test]
    fn test_loan_count_is_per_borrower() {
        let env = Env::default();
        let (contract_id, token_addr, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        let borrower2 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.vouch(&voucher2, &borrower2, &1_000_000);

        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(client.loan_count(&borrower), 1);
        assert_eq!(client.loan_count(&borrower2), 0);
    }

    // ── Default Count Tests ───────────────────────────────────────────────────

    #[test]
    fn test_default_count_zero_for_new_borrower() {
        let env = Env::default();
        let (contract_id, _, _, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        assert_eq!(client.default_count(&borrower), 0);
    }

    #[test]
    fn test_default_count_increments_on_slash() {
        let env = Env::default();
        let (contract_id, _, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        assert_eq!(client.default_count(&borrower), 0);

        client.slash(&admin_signers, &borrower);
        assert_eq!(client.default_count(&borrower), 1);
    }

    #[test]
    fn test_default_count_increments_on_auto_slash() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.loan_duration = 1_000;
        client.set_config(&admin_signers, &cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        env.ledger().set_timestamp(1_002_000);
        client.auto_slash(&borrower);
        assert_eq!(client.default_count(&borrower), 1);
    }

    #[test]
    fn test_default_count_increments_on_claim_expired_loan() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, _, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.loan_duration = 1_000;
        client.set_config(&admin_signers, &cfg);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);

        env.ledger().set_timestamp(1_002_000);
        client.claim_expired_loan(&borrower);
        assert_eq!(client.default_count(&borrower), 1);
    }

    #[test]
    fn test_default_count_not_incremented_on_repay() {
        let env = Env::default();
        let (contract_id, _, _, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        assert_eq!(client.default_count(&borrower), 0);
    }

    #[test]
    fn test_default_count_is_per_borrower() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let borrower2 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &10_000_000);

        client.vouch(&voucher, &borrower, &1_000_000);
        client.vouch(&voucher2, &borrower2, &1_000_000);

        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower);

        assert_eq!(client.default_count(&borrower), 1);
        assert_eq!(client.default_count(&borrower2), 0);
    }

    // ── Reputation NFT Tests ──────────────────────────────────────────────────

    #[test]
    fn test_repay_mints_reputation() {
        let env = Env::default();
        let (contract_id, _token, _admin, borrower, voucher, nft_id) = setup_with_reputation(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let nft = reputation::ReputationNftContractClient::new(&env, &nft_id);

        assert_eq!(client.get_reputation(&borrower), 0);

        client.vouch(&voucher, &borrower, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);

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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        assert_eq!(client.repayment_count(&borrower), 1);

        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        assert_eq!(nft.balance(&borrower), 1);

        let borrower2 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &2_000_000);

        nft.mint(&borrower2);
        assert_eq!(nft.balance(&borrower2), 1);

        client.vouch(&voucher2, &borrower2, &1_000_000);
        advance_past_vouch_age(&env);
        client.request_loan(&borrower2, &Vec::new(&env), &500_000, &1_000_000);
        client.slash(&admin_signers, &borrower2);

        assert_eq!(client.get_reputation(&borrower2), 0);
        assert_eq!(nft.balance(&borrower2), 0);
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

        assert_eq!(client.get_loan(&borrower1).unwrap().amount, 500_000);
        assert_eq!(client.get_loan(&borrower2).unwrap().amount, 300_000);
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
        advance_past_vouch_age(&env);
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &2_000_000);

        let mut borrowers = Vec::new(&env);
        borrowers.push_back(borrower);
        let mut amounts = Vec::new(&env);
        amounts.push_back(500_000i128);

        let result = client.try_create_loan_pool(&admin_signers, &borrowers, &amounts);
        assert_eq!(result, Err(Ok(ContractError::PoolBorrowerActiveLoan)));
    }

    #[test]
    fn test_get_loan_pool_unknown_returns_none() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        assert!(client.get_loan_pool(&999u64).is_none());
    }

    // ── Upgrade Tests ─────────────────────────────────────────────────────────

    #[test]
    #[should_panic(expected = "insufficient admin approvals")]
    fn test_upgrade_rejected_without_admin_approval() {
        let env = Env::default();
        let (contract_id, _token_addr, _admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let outsider = Address::generate(&env);
        let fake_hash = BytesN::from_array(&env, &[0u8; 32]);
        client.upgrade(&single_admin_signers(&env, &outsider), &fake_hash);
    }

    #[test]
    #[should_panic(expected = "insufficient admin approvals")]
    fn test_upgrade_multisig_rejects_single_signer() {
        let env = Env::default();
        let (contract_id, _token_addr, admin_one, _admin_two, _admin_three, _borrower, _voucher) =
            setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let fake_hash = BytesN::from_array(&env, &[0u8; 32]);
        client.upgrade(&single_admin_signers(&env, &admin_one), &fake_hash);
    }

    // ── Rate Limiting: vouch cooldown ─────────────────────────────────────────

    #[test]
    fn test_vouch_cooldown_blocks_second_vouch_within_window() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.vouch_cooldown_secs = 3_600;
        client.set_config(&admin_signers, &cfg);

        let voucher = Address::generate(&env);
        let borrower1 = Address::generate(&env);
        let borrower2 = Address::generate(&env);
        token_admin.mint(&voucher, &2_000_000);

        client.vouch(&voucher, &borrower1, &1_000_000);

        let result = client.try_vouch(&voucher, &borrower2, &1_000_000);
        assert_eq!(result, Err(Ok(ContractError::VouchCooldownActive)));
    }

    #[test]
    fn test_vouch_cooldown_allows_vouch_after_window_expires() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.vouch_cooldown_secs = 3_600;
        client.set_config(&admin_signers, &cfg);

        let voucher = Address::generate(&env);
        let borrower1 = Address::generate(&env);
        let borrower2 = Address::generate(&env);
        token_admin.mint(&voucher, &2_000_000);

        client.vouch(&voucher, &borrower1, &1_000_000);

        env.ledger().with_mut(|l| l.timestamp += 3_601);

        client.vouch(&voucher, &borrower2, &1_000_000);
        assert!(client.vouch_exists(&voucher, &borrower2));
    }

    #[test]
    fn test_vouch_cooldown_zero_disables_rate_limit() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.vouch_cooldown_secs = 0;
        client.set_config(&admin_signers, &cfg);

        let voucher = Address::generate(&env);
        let borrower1 = Address::generate(&env);
        let borrower2 = Address::generate(&env);
        token_admin.mint(&voucher, &2_000_000);

        client.vouch(&voucher, &borrower1, &1_000_000);
        client.vouch(&voucher, &borrower2, &1_000_000);
        assert!(client.vouch_exists(&voucher, &borrower2));
    }

    #[test]
    fn test_vouch_cooldown_is_per_voucher_not_global() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000_000);
        let (contract_id, token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        let mut cfg = client.get_config();
        cfg.vouch_cooldown_secs = 3_600;
        client.set_config(&admin_signers, &cfg);

        let voucher_a = Address::generate(&env);
        let voucher_b = Address::generate(&env);
        let borrower = Address::generate(&env);
        token_admin.mint(&voucher_a, &1_000_000);
        token_admin.mint(&voucher_b, &1_000_000);

        client.vouch(&voucher_a, &borrower, &1_000_000);

        // voucher_b has never vouched — must succeed immediately despite voucher_a's cooldown
        client.vouch(&voucher_b, &borrower, &1_000_000);
        assert!(client.vouch_exists(&voucher_b, &borrower));
    }

    // ── Multisig Admin Security Tests ─────────────────────────────────────────

    // --- require_admin_approval enforcement ---

    #[test]
    #[should_panic(expected = "insufficient admin approvals")]
    fn test_admin_op_fails_with_zero_signers() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        // Provide no signers — must fail threshold check
        client.pause(&Vec::new(&env));
    }

    #[test]
    #[should_panic(expected = "insufficient admin approvals")]
    fn test_admin_op_fails_below_threshold() {
        let env = Env::default();
        let (contract_id, _, admin_one, _, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        // Only 1 signer for a 2-of-3 contract
        let signers = address_vec(&env, &[admin_one]);
        client.pause(&signers);
    }

    #[test]
    #[should_panic(expected = "signer is not a registered admin")]
    fn test_admin_op_fails_with_non_admin_signer() {
        let env = Env::default();
        let (contract_id, _, admin_one, _, _, _, _) = setup_multisig(&env, 1);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let outsider = Address::generate(&env);
        // outsider is not in the admin set
        let signers = address_vec(&env, &[admin_one, outsider]);
        client.pause(&signers);
    }

    #[test]
    fn test_admin_op_succeeds_at_exact_threshold() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);
        client.pause(&signers);
        assert!(client.get_paused());
    }

    #[test]
    fn test_admin_op_succeeds_above_threshold() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, admin_three, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        // 3 signers for a 2-of-3 — all three signing is fine
        let signers = address_vec(&env, &[admin_one, admin_two, admin_three]);
        client.pause(&signers);
        assert!(client.get_paused());
    }

    // --- add_admin ---

    #[test]
    fn test_add_admin_increases_admin_count() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);

        let new_admin = Address::generate(&env);
        assert_eq!(client.get_admins().len(), 3);
        client.add_admin(&signers, &new_admin);
        assert_eq!(client.get_admins().len(), 4);
        assert!(client.get_admins().iter().any(|a| a == new_admin));
    }

    #[test]
    #[should_panic(expected = "address is already an admin")]
    fn test_add_admin_rejects_duplicate() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, admin_three, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);
        // admin_three is already in the set
        client.add_admin(&signers, &admin_three);
    }

    #[test]
    #[should_panic(expected = "insufficient admin approvals")]
    fn test_add_admin_requires_quorum() {
        let env = Env::default();
        let (contract_id, _, admin_one, _, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let new_admin = Address::generate(&env);
        // Only 1 signer for a 2-of-3 contract
        client.add_admin(&address_vec(&env, &[admin_one]), &new_admin);
    }

    // --- remove_admin ---

    #[test]
    fn test_remove_admin_decreases_admin_count() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, admin_three, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);

        assert_eq!(client.get_admins().len(), 3);
        client.remove_admin(&signers, &admin_three);
        assert_eq!(client.get_admins().len(), 2);
        assert!(!client.get_admins().iter().any(|a| a == admin_three));
    }

    #[test]
    #[should_panic(expected = "removal would make threshold unsatisfiable")]
    fn test_remove_admin_blocked_when_threshold_would_be_unsatisfiable() {
        let env = Env::default();
        // 3-of-3: removing any admin makes threshold unsatisfiable
        let (contract_id, _, admin_one, admin_two, admin_three, _, _) = setup_multisig(&env, 3);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two, admin_three.clone()]);
        client.remove_admin(&signers, &admin_three);
    }

    #[test]
    #[should_panic(expected = "address is not an admin")]
    fn test_remove_admin_rejects_unknown_address() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);
        let outsider = Address::generate(&env);
        client.remove_admin(&signers, &outsider);
    }

    #[test]
    #[should_panic(expected = "insufficient admin approvals")]
    fn test_remove_admin_requires_quorum() {
        let env = Env::default();
        let (contract_id, _, admin_one, _, admin_three, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        // Only 1 signer for a 2-of-3 contract
        client.remove_admin(&address_vec(&env, &[admin_one]), &admin_three);
    }

    // --- rotate_admin ---

    #[test]
    fn test_rotate_admin_replaces_key_preserves_count() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, admin_three, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);

        let new_key = Address::generate(&env);
        client.rotate_admin(&signers, &admin_three, &new_key);

        let admins = client.get_admins();
        assert_eq!(admins.len(), 3);
        assert!(!admins.iter().any(|a| a == admin_three));
        assert!(admins.iter().any(|a| a == new_key));
        // threshold unchanged
        assert_eq!(client.get_admin_threshold(), 2);
    }

    #[test]
    #[should_panic(expected = "old and new admin must differ")]
    fn test_rotate_admin_rejects_same_address() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, admin_three, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);
        client.rotate_admin(&signers, &admin_three, &admin_three);
    }

    #[test]
    #[should_panic(expected = "new admin is already in the admin set")]
    fn test_rotate_admin_rejects_existing_admin_as_new() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, admin_three, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one.clone(), admin_two]);
        // admin_one is already in the set — cannot be the "new" key
        client.rotate_admin(&signers, &admin_three, &admin_one);
    }

    #[test]
    #[should_panic(expected = "old admin not found")]
    fn test_rotate_admin_rejects_unknown_old_admin() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);
        let outsider = Address::generate(&env);
        let new_key = Address::generate(&env);
        client.rotate_admin(&signers, &outsider, &new_key);
    }

    #[test]
    #[should_panic(expected = "insufficient admin approvals")]
    fn test_rotate_admin_requires_quorum() {
        let env = Env::default();
        let (contract_id, _, admin_one, _, admin_three, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let new_key = Address::generate(&env);
        client.rotate_admin(&address_vec(&env, &[admin_one]), &admin_three, &new_key);
    }

    // --- set_admin_threshold ---

    #[test]
    fn test_set_admin_threshold_updates_value() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);

        assert_eq!(client.get_admin_threshold(), 2);
        client.set_admin_threshold(&signers, &3);
        assert_eq!(client.get_admin_threshold(), 3);
    }

    #[test]
    #[should_panic(expected = "threshold must be greater than zero")]
    fn test_set_admin_threshold_zero_rejected() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);
        client.set_admin_threshold(&signers, &0);
    }

    #[test]
    #[should_panic(expected = "threshold cannot exceed admin count")]
    fn test_set_admin_threshold_exceeds_admin_count_rejected() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one, admin_two]);
        // 3 admins, threshold of 4 is impossible
        client.set_admin_threshold(&signers, &4);
    }

    #[test]
    #[should_panic(expected = "insufficient admin approvals")]
    fn test_set_admin_threshold_requires_quorum() {
        let env = Env::default();
        let (contract_id, _, admin_one, _, _, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.set_admin_threshold(&address_vec(&env, &[admin_one]), &1);
    }

    // --- end-to-end key rotation scenario ---

    #[test]
    fn test_key_rotation_new_admin_can_execute_operations() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, admin_three, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one.clone(), admin_two.clone()]);

        // Rotate admin_three out for a fresh key
        let new_key = Address::generate(&env);
        client.rotate_admin(&signers, &admin_three, &new_key);

        // New key + admin_one should now satisfy the 2-of-3 threshold
        let new_signers = address_vec(&env, &[admin_one, new_key]);
        client.pause(&new_signers);
        assert!(client.get_paused());
    }

    #[test]
    fn test_rotated_out_admin_cannot_execute_operations() {
        let env = Env::default();
        let (contract_id, _, admin_one, admin_two, admin_three, _, _) = setup_multisig(&env, 2);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let signers = address_vec(&env, &[admin_one.clone(), admin_two.clone()]);

        let new_key = Address::generate(&env);
        client.rotate_admin(&signers, &admin_three, &new_key);

        // admin_three is no longer in the set — using it should fail
        let stale_signers = address_vec(&env, &[admin_one, admin_three]);
        let result = client.try_pause(&stale_signers);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_admin_config_rejects_empty_admins() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);
        let result = QuorumCreditContractClient::new(&env, &contract_id)
            .try_initialize(&admin, &Vec::new(&env), &1, &token_id.address());
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_admin_config_rejects_duplicate_admins() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);
        // Pass the same address twice
        let dup_admins = address_vec(&env, &[admin.clone(), admin.clone()]);
        let result = QuorumCreditContractClient::new(&env, &contract_id)
            .try_initialize(&admin, &dup_admins, &1, &token_id.address());
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_admin_config_rejects_threshold_exceeding_admin_count() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);
        let admins = address_vec(&env, &[admin.clone()]);
        // threshold 2 with only 1 admin
        let result = QuorumCreditContractClient::new(&env, &contract_id)
            .try_initialize(&admin, &admins, &2, &token_id.address());
        assert!(result.is_err());
    }

    #[test]
    fn test_stake_overflow_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let borrower = Address::generate(&env);
        let v1 = Address::generate(&env);
        let v2 = Address::generate(&env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);
        QuorumCreditContractClient::new(&env, &contract_id)
            .initialize(&admin, &admin, &token_id.address());

        // Directly write two vouches whose stakes overflow i128 when summed,
        // bypassing token transfer so we can use values > token balance limits.
        let big_stake = i128::MAX / 2 + 1;
        let vouches = soroban_sdk::vec![
            &env,
            VouchRecord { voucher: v1, stake: big_stake },
            VouchRecord { voucher: v2, stake: big_stake },
        ];
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::Vouches(borrower.clone()), &vouches);
        });

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let result = client.try_request_loan(&borrower, &1, &1);
        assert_eq!(
            result,
            Err(Ok(ContractError::StakeOverflow)),
            "expected StakeOverflow on i128 overflow in stake summation"
        );
    }

    #[test]
    fn test_stake_overflow_rejected() {        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let borrower = Address::generate(&env);
        let v1 = Address::generate(&env);
        let v2 = Address::generate(&env);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);
        QuorumCreditContractClient::new(&env, &contract_id)
            .initialize(&admin, &admin, &token_id.address());

        // Directly write two vouches whose stakes overflow i128 when summed,
        // bypassing token transfer so we can use values > token balance limits.
        let big_stake = i128::MAX / 2 + 1;
        let vouches = soroban_sdk::vec![
            &env,
            VouchRecord { voucher: v1, stake: big_stake },
            VouchRecord { voucher: v2, stake: big_stake },
        ];
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::Vouches(borrower.clone()), &vouches);
        });

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let result = client.try_request_loan(&borrower, &1, &1);
        assert_eq!(
            result,
            Err(Ok(ContractError::StakeOverflow)),
            "expected StakeOverflow on i128 overflow in stake summation"
        );
    }

    #[test]
    fn test_initialize_rejects_zero_admin() {
        let env = Env::default();
        env.mock_all_auths();

        let deployer = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(deployer.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);

        // All-zeros account strkey
        let zero_admin = Address::from_string(
            &soroban_sdk::String::from_str(&env, "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"),
        );

        let result = QuorumCreditContractClient::new(&env, &contract_id)
            .try_initialize(&deployer, &zero_admin, &token_id.address());

        assert_eq!(result, Err(Ok(ContractError::ZeroAddress)));
    }

    #[test]
    fn test_initialize_rejects_zero_token() {
        let env = Env::default();
        env.mock_all_auths();

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let contract_id = env.register_contract(None, QuorumCreditContract);

        // All-zeros contract strkey
        let zero_token = Address::from_string(
            &soroban_sdk::String::from_str(&env, "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"),
        );

        let result = QuorumCreditContractClient::new(&env, &contract_id)
            .try_initialize(&deployer, &admin, &zero_token);

        assert_eq!(result, Err(Ok(ContractError::ZeroAddress)));
    }
}
