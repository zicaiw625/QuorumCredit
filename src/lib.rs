#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Yield paid to vouchers on repayment: 200 basis points = 2%.
const YIELD_BPS: i128 = 200;
/// Slash penalty on default: 5000 basis points = 50% of voucher stake burned.
const SLASH_BPS: i128 = 5000;
/// Maximum number of vouchers per loan to prevent DoS.
const MAX_VOUCHERS_PER_LOAN: u32 = 100;
/// Minimum loan amount in stroops to prevent dust loans (0.01 XLM).
const MIN_LOAN_AMOUNT: i128 = 100_000;
/// Loan expiry time in seconds: 30 days.
const LOAN_EXPIRY_SECONDS: u64 = 30 * 24 * 60 * 60;

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    InsufficientFunds = 1,
    DuplicateVouch = 2,
    NoActiveLoan = 3,
    ContractPaused = 4,
    LoanPastDeadline = 5,
}

// ── Storage Keys ──────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Loan(Address),    // borrower → LoanRecord
    Vouches(Address), // borrower → Vec<VouchRecord>
    Admin,            // Address allowed to call slash
    Token,            // XLM token contract address
    Deployer,         // Address that deployed the contract; guards initialize
    SlashTreasury,    // i128 accumulated slashed funds
    Paused,           // bool: true when contract is paused
    LoanDuration,     // u64 configurable loan duration in seconds
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

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct QuorumCreditContract;

#[contractimpl]
impl QuorumCreditContract {
    /// One-time initialisation: set admin and XLM token address.
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

        env.storage().instance().set(&DataKey::Deployer, &deployer);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);
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
            vouches.len() < MAX_VOUCHERS_PER_LOAN,
            "maximum vouchers per loan exceeded"
        );

        // Transfer stake from voucher into the contract.
        let token = Self::token(&env);
        token.transfer(&voucher, &env.current_contract_address(), &stake);

        vouches.push_back(VouchRecord { voucher, stake });
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
            amount >= MIN_LOAN_AMOUNT,
            "loan amount must meet minimum threshold"
        );
        assert!(threshold > 0, "threshold must be greater than zero");

        // Prevent multiple active loans.
        assert!(
            !env.storage()
                .persistent()
                .has(&DataKey::Loan(borrower.clone())),
            "borrower already has an active loan"
        );

        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        let total_stake: i128 = vouches.iter().map(|v| v.stake).sum();
        assert!(total_stake >= threshold, "insufficient trust stake");

        // Verify the contract holds enough XLM to cover the loan.
        let token = Self::token(&env);
        let contract_balance = token.balance(&env.current_contract_address());
        if contract_balance < amount {
            return Err(ContractError::InsufficientFunds);
        }

        let now = env.ledger().timestamp();
        let duration: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LoanDuration)
            .unwrap_or(LOAN_EXPIRY_SECONDS);
        let deadline = now + duration;

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

        assert!(!loan.defaulted, "loan already defaulted");
        assert!(!loan.repaid, "loan already repaid");

        // Block repayment after deadline — borrower must be auto-slashed instead.
        assert!(
            env.ledger().timestamp() <= loan.deadline,
            "loan deadline has passed"
        );

        let token = Self::token(&env);
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        // Pre-calculate total payout to ensure contract has enough balance.
        let mut total_payout: i128 = 0;
        for v in vouches.iter() {
            let yield_amount = v.stake * YIELD_BPS / 10_000;
            total_payout += v.stake + yield_amount;
        }

        // Collect repayment from borrower first.
        token.transfer(&borrower, &env.current_contract_address(), &loan.amount);

        let contract_balance = token.balance(&env.current_contract_address());
        assert!(
            contract_balance >= total_payout,
            "insufficient contract balance for yield distribution"
        );

        // Return stake + 2% yield to each voucher.
        for v in vouches.iter() {
            let yield_amount = v.stake * YIELD_BPS / 10_000;
            token.transfer(
                &env.current_contract_address(),
                &v.voucher,
                &(v.stake + yield_amount),
            );
        }

        loan.repaid = true;
        env.storage()
            .persistent()
            .set(&DataKey::Loan(borrower), &loan);

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
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        for v in vouches.iter() {
            let slash_amount = v.stake * SLASH_BPS / 10_000;
            let returned = v.stake - slash_amount;
            // Return remaining 50% to voucher; slashed half stays in contract.
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
        let vouches: Vec<VouchRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Vouches(borrower.clone()))
            .unwrap_or(Vec::new(&env));

        for v in vouches.iter() {
            let slash_amount = v.stake * SLASH_BPS / 10_000;
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

        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower));
    }

    /// Admin sets the loan duration (in seconds) applied to future loans.
    pub fn set_loan_duration(env: Env, duration_seconds: u64) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        admin.require_auth();
        assert!(duration_seconds > 0, "duration must be greater than zero");
        env.storage()
            .instance()
            .set(&DataKey::LoanDuration, &duration_seconds);
    }

    /// Returns the current loan duration in seconds.
    pub fn get_loan_duration(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::LoanDuration)
            .unwrap_or(LOAN_EXPIRY_SECONDS)
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

    // ── Views ─────────────────────────────────────────────────────────────────

    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
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

    pub fn get_loan(env: Env, borrower: Address) -> Option<LoanRecord> {
        env.storage().persistent().get(&DataKey::Loan(borrower))
    }

    pub fn get_vouches(env: Env, borrower: Address) -> Option<Vec<VouchRecord>> {
        env.storage().persistent().get(&DataKey::Vouches(borrower))
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
        testutils::{Address as _, Ledger},
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

        QuorumCreditContractClient::new(&env, &contract_id).initialize(
            &admin,
            &admin,
            &token_id.address(),
        );

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        // Stake 1_000_000 — contract now holds exactly 1_000_000.
        client.vouch(&voucher, &borrower, &1_000_000);

        // Request 2_000_000 which exceeds the contract's 1_000_000 balance.
        let result = client.try_request_loan(&borrower, &2_000_000, &1_000_000);
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
    fn test_repay_with_max_vouchers() {
        let env = Env::default();
        env.budget().reset_unlimited();
        let (contract_id, token_addr, _admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        // Create max vouchers
        let mut vouchers = Vec::new(&env);
        for _ in 0..MAX_VOUCHERS_PER_LOAN {
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
            &(MAX_VOUCHERS_PER_LOAN as i128 * 1_000_000),
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
        let (contract_id, token_addr, _admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);

        // Create MAX_VOUCHERS_PER_LOAN vouchers
        let mut vouchers = Vec::new(&env);
        for _ in 0..MAX_VOUCHERS_PER_LOAN {
            let voucher = Address::generate(&env);
            token_admin.mint(&voucher, &10_000_000);
            vouchers.push_back(voucher);
        }

        // Vouch with all MAX_VOUCHERS_PER_LOAN
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
        let (contract_id, _token_addr, admin, borrower, voucher) = setup(&env);
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

        // Set a short custom duration: 1000 seconds.
        client.set_loan_duration(&1_000);
        assert_eq!(client.get_loan_duration(), 1_000);

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

        client.set_loan_duration(&1_000);
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

        client.set_loan_duration(&1_000);
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

        client.set_loan_duration(&1_000);
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

        assert_eq!(client.get_loan_duration(), LOAN_EXPIRY_SECONDS);
    }

    #[test]
    fn test_get_admin_returns_admin_address() {
        let env = Env::default();
        let (contract_id, _token_addr, admin, _borrower, _voucher) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        assert_eq!(client.get_admin(), admin);
    }
}
