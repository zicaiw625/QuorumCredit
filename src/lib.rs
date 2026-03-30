#![no_std]

use soroban_sdk::{
    contract, contractimpl, panic_with_error, symbol_short, Address, BytesN, Env, Vec,
};

pub mod admin;
pub mod errors;
pub mod governance;
pub mod helpers;
pub mod loan;
pub mod reputation;
pub mod types;
pub mod vouch;

// #[cfg(test)]
mod governance_test;
// #[cfg(test)]
mod initialize_test;
// #[cfg(test)]
mod loan_purpose_test;
// #[cfg(test)]
mod multi_asset_test;
// #[cfg(test)]
mod referral_test;
// #[cfg(test)]
#[cfg(test)]
mod min_loan_amount_test;
mod request_loan_insufficient_stake_test;
mod security_fixes_test;
#[cfg(test)]
mod vouch_zero_stake_test;
// #[cfg(test)]
mod bug_condition_test;
#[cfg(test)]
mod double_slash_panic_test;
#[cfg(test)]
mod duplicate_loan_test;
#[cfg(test)]
mod full_lifecycle_test;

#[cfg(test)]
mod get_loan_none_test;

// #[cfg(test)]
#[cfg(test)]
mod loan_overwrite_protection_test;
#[cfg(test)]
mod max_vouchers_per_borrower_test;
#[cfg(test)]
mod paused_state_test;
#[cfg(test)]
mod repay_nonexistent_loan_test;
#[cfg(test)]
mod partial_repay_test;
mod slash_multi_voucher_test;
#[cfg(test)]
mod voucher_balance_check_test;
#[cfg(test)]
mod vouch_cooldown_test;
#[cfg(test)]
mod repay_protocol_fee_test;
#[cfg(test)]
mod is_eligible_token_filter_test;
#[cfg(test)]
mod vote_slash_auto_execute_test;

pub use errors::ContractError;
pub use types::*;

use helpers::{require_valid_token, validate_admin_config};
use reputation::ReputationNftExternalClient;

#[contract]
pub struct QuorumCreditContract;

#[contractimpl]
impl QuorumCreditContract {
    /// One-time contract initialization. Deployer must sign.
    pub fn initialize(
        env: Env,
        deployer: Address,
        admins: Vec<Address>,
        admin_threshold: u32,
        token: Address,
    ) -> Result<(), ContractError> {
        deployer.require_auth();

        if env.storage().instance().has(&DataKey::Config) {
            panic_with_error!(&env, ContractError::AlreadyInitialized);
        }

        validate_admin_config(&env, &admins, admin_threshold).expect("invalid admin config");
        require_valid_token(&env, &token).expect("invalid token");
        assert!(
            !env.storage().instance().has(&DataKey::Config),
            "already initialized"
        );

        validate_admin_config(&env, &admins, admin_threshold)?;
        require_valid_token(&env, &token)?;

        env.storage().instance().set(&DataKey::Deployer, &deployer);
        env.storage().instance().set(
            &DataKey::Config,
            &Config {
                admins: admins.clone(),
                admin_threshold,
                token: token.clone(),
                allowed_tokens: Vec::new(&env),
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
            (deployer, admins, admin_threshold, token),
        );
        Ok(())
    }

    // ── Vouch ─────────────────────────────────────────────────────────────────

    /// Vouch for a borrower by staking tokens.
    ///
    /// # Arguments
    /// * `voucher` - Address of the voucher staking tokens
    /// * `borrower` - Address of the borrower being vouched for
    /// * `stake` - Amount of tokens to stake (must be positive)
    /// * `token` - Address of the token contract to stake
    ///
    /// # Panics
    /// * If voucher is the same as borrower
    /// * If stake is not greater than zero
    /// * If token is not allowed
    /// * If minimum stake requirement is not met
    /// * If borrower has an active loan
    /// * If duplicate vouch from same voucher for same borrower
    /// * If contract is paused
    pub fn vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
        stake: i128,
        token: Address,
    ) -> Result<(), ContractError> {
        vouch::vouch(env, voucher, borrower, stake, token)
    }

    /// Vouch for multiple borrowers in a single transaction.
    ///
    /// # Arguments
    /// * `voucher` - Address of the voucher staking tokens
    /// * `borrowers` - Vector of borrower addresses
    /// * `stakes` - Vector of stake amounts (must match borrowers length)
    /// * `token` - Address of the token contract to stake
    ///
    /// # Panics
    /// * If borrowers and stakes vectors have different lengths
    /// * If batch is empty
    /// * If any individual vouch fails (see `vouch` function)
    pub fn batch_vouch(
        env: Env,
        voucher: Address,
        borrowers: Vec<Address>,
        stakes: Vec<i128>,
        token: Address,
    ) -> Result<(), ContractError> {
        vouch::batch_vouch(env, voucher, borrowers, stakes, token)
    }

    /// Increase the stake for an existing vouch.
    ///
    /// # Arguments
    /// * `voucher` - Address of the voucher
    /// * `borrower` - Address of the borrower
    /// * `additional` - Additional amount to stake (must be positive)
    ///
    /// # Panics
    /// * If vouch does not exist
    /// * If additional amount is not positive
    /// * If contract is paused
    pub fn increase_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        additional: i128,
    ) -> Result<(), ContractError> {
        vouch::increase_stake(env, voucher, borrower, additional)
    }

    /// Decrease the stake for an existing vouch.
    ///
    /// # Arguments
    /// * `voucher` - Address of the voucher
    /// * `borrower` - Address of the borrower
    /// * `amount` - Amount to decrease (must be positive and not exceed current stake)
    ///
    /// # Panics
    /// * If vouch does not exist
    /// * If amount is not positive
    /// * If amount exceeds current stake
    /// * If borrower has an active loan
    /// * If contract is paused
    pub fn decrease_stake(
        env: Env,
        voucher: Address,
        borrower: Address,
        amount: i128,
    ) -> Result<(), ContractError> {
        vouch::decrease_stake(env, voucher, borrower, amount)
    }

    /// Withdraw a vouch completely and return the stake to the voucher.
    ///
    /// # Arguments
    /// * `voucher` - Address of the voucher
    /// * `borrower` - Address of the borrower
    ///
    /// # Panics
    /// * If vouch does not exist
    /// * If borrower has an active loan
    /// * If contract is paused
    pub fn withdraw_vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        vouch::withdraw_vouch(env, voucher, borrower)
    }

    /// Transfer a vouch from one address to another.
    ///
    /// # Arguments
    /// * `from` - Address of the current voucher
    /// * `to` - Address of the new voucher
    /// * `borrower` - Address of the borrower
    ///
    /// # Panics
    /// * If vouch does not exist for `from`
    /// * If borrower has an active loan
    /// * If contract is paused
    pub fn transfer_vouch(
        env: Env,
        from: Address,
        to: Address,
        borrower: Address,
    ) -> Result<(), ContractError> {
        vouch::transfer_vouch(env, from, to, borrower)
    }

    // ── Loan ──────────────────────────────────────────────────────────────────

    /// Register a referrer for a borrower. Must be called before `request_loan`.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    /// * `referrer` - Address of the referrer (cannot be the borrower)
    ///
    /// # Panics
    /// * If borrower is the same as referrer
    /// * If borrower has an active loan
    /// * If contract is paused
    pub fn register_referral(
        env: Env,
        borrower: Address,
        referrer: Address,
    ) -> Result<(), ContractError> {
        loan::register_referral(env, borrower, referrer)
    }

    /// Get the referrer for a borrower.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `Option<Address>` - The referrer address if set, None otherwise
    pub fn get_referrer(env: Env, borrower: Address) -> Option<Address> {
        loan::get_referrer(env, borrower)
    }

    /// Set the referral bonus in basis points.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `bonus_bps` - Bonus in basis points (must not exceed 10000)
    ///
    /// # Panics
    /// * If admin approval is insufficient
    /// * If bonus_bps exceeds 10000
    pub fn set_referral_bonus_bps(env: Env, admin_signers: Vec<Address>, bonus_bps: u32) {
        helpers::require_admin_approval(&env, &admin_signers);
        assert!(bonus_bps <= 10_000, "bonus_bps must not exceed 10000");
        env.storage()
            .instance()
            .set(&DataKey::ReferralBonusBps, &bonus_bps);
    }

    /// Get the current referral bonus in basis points.
    ///
    /// # Returns
    /// * `u32` - The referral bonus in basis points
    pub fn get_referral_bonus_bps(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ReferralBonusBps)
            .unwrap_or(crate::types::DEFAULT_REFERRAL_BONUS_BPS)
    }

    /// Request a loan from the protocol.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    /// * `amount` - Loan amount in stroops
    /// * `threshold` - Minimum total stake required from vouchers
    /// * `loan_purpose` - Description of the loan purpose
    /// * `token` - Address of the token contract for the loan
    ///
    /// # Panics
    /// * If borrower is blacklisted
    /// * If token is not allowed
    /// * If amount is below minimum loan amount
    /// * If threshold is not positive
    /// * If amount exceeds maximum loan amount
    /// * If borrower has an active loan
    /// * If total vouched stake is below threshold
    /// * If number of vouchers is below minimum
    /// * If any vouch is too recent
    /// * If loan amount exceeds maximum collateral ratio
    /// * If contract has insufficient balance
    /// * If contract is paused
    pub fn request_loan(
        env: Env,
        borrower: Address,
        amount: i128,
        threshold: i128,
        loan_purpose: soroban_sdk::String,
        token: Address,
    ) -> Result<(), ContractError> {
        loan::request_loan(env, borrower, amount, threshold, loan_purpose, token)
    }

    /// Repay a loan partially or fully.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    /// * `payment` - Payment amount in stroops (must be positive and not exceed outstanding balance)
    ///
    /// # Panics
    /// * If borrower does not have an active loan
    /// * If loan deadline has passed
    /// * If payment is not positive
    /// * If payment exceeds outstanding balance
    /// * If contract is paused
    pub fn repay(env: Env, borrower: Address, payment: i128) -> Result<(), ContractError> {
        loan::repay(env, borrower, payment)
    }

    // ── Admin ─────────────────────────────────────────────────────────────────

    /// Add a new admin to the protocol.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `new_admin` - Address of the new admin to add
    ///
    /// # Panics
    /// * If admin approval is insufficient
    /// * If new admin already exists
    /// * If new admin is a zero address
    pub fn add_admin(env: Env, admin_signers: Vec<Address>, new_admin: Address) {
        admin::add_admin(env, admin_signers, new_admin)
    }

    /// Remove an admin from the protocol.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `admin_to_remove` - Address of the admin to remove
    ///
    /// # Panics
    /// * If admin approval is insufficient
    /// * If admin to remove does not exist
    /// * If removal would leave fewer admins than threshold
    pub fn remove_admin(env: Env, admin_signers: Vec<Address>, admin_to_remove: Address) {
        admin::remove_admin(env, admin_signers, admin_to_remove)
    }

    /// Rotate an admin address to a new address.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `old_admin` - Address of the admin to replace
    /// * `new_admin` - Address of the new admin
    ///
    /// # Panics
    /// * If admin approval is insufficient
    /// * If old admin does not exist
    /// * If new admin already exists
    /// * If new admin is a zero address
    pub fn rotate_admin(
        env: Env,
        admin_signers: Vec<Address>,
        old_admin: Address,
        new_admin: Address,
    ) {
        admin::rotate_admin(env, admin_signers, old_admin, new_admin)
    }

    /// Set the admin threshold (minimum number of admins required for approval).
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet current threshold)
    /// * `new_threshold` - New threshold value (must be > 0 and <= admin count)
    ///
    /// # Panics
    /// * If admin approval is insufficient
    /// * If new_threshold is 0
    /// * If new_threshold exceeds admin count
    pub fn set_admin_threshold(env: Env, admin_signers: Vec<Address>, new_threshold: u32) {
        admin::set_admin_threshold(env, admin_signers, new_threshold)
    }

    /// Set the protocol fee in basis points.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `fee_bps` - Fee in basis points (must not exceed 10000)
    ///
    /// # Panics
    /// * If admin approval is insufficient
    /// * If fee_bps exceeds 10000
    pub fn set_protocol_fee(env: Env, admin_signers: Vec<Address>, fee_bps: u32) {
        admin::set_protocol_fee(env, admin_signers, fee_bps)
    }

    /// Whitelist a voucher to allow them to vouch.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `voucher` - Address of the voucher to whitelist
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn whitelist_voucher(env: Env, admin_signers: Vec<Address>, voucher: Address) {
        admin::whitelist_voucher(env, admin_signers, voucher)
    }

    /// Set the fee treasury address where protocol fees are sent.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `treasury` - Address of the fee treasury
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn set_fee_treasury(env: Env, admin_signers: Vec<Address>, treasury: Address) {
        admin::set_fee_treasury(env, admin_signers, treasury)
    }

    /// Upgrade the contract to a new WASM hash.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `new_wasm_hash` - Hash of the new WASM code
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn upgrade(env: Env, admin_signers: Vec<Address>, new_wasm_hash: BytesN<32>) {
        admin::upgrade(env, admin_signers, new_wasm_hash)
    }

    /// Pause the contract (stops all operations except admin functions).
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn pause(env: Env, admin_signers: Vec<Address>) {
        admin::pause(env, admin_signers)
    }

    /// Unpause the contract (resumes all operations).
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn unpause(env: Env, admin_signers: Vec<Address>) {
        admin::unpause(env, admin_signers)
    }

    /// Blacklist a borrower (prevents them from requesting loans).
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `borrower` - Address of the borrower to blacklist
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn blacklist(env: Env, admin_signers: Vec<Address>, borrower: Address) {
        admin::blacklist(env, admin_signers, borrower)
    }

    /// Set the entire protocol configuration.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `config` - New configuration struct
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn set_config(env: Env, admin_signers: Vec<Address>, config: Config) {
        admin::set_config(env, admin_signers, config)
    }

    /// Update specific configuration parameters.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `yield_bps` - New yield in basis points (optional)
    /// * `slash_bps` - New slash in basis points (optional)
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn update_config(
        env: Env,
        admin_signers: Vec<Address>,
        yield_bps: Option<i128>,
        slash_bps: Option<i128>,
    ) {
        admin::update_config(env, admin_signers, yield_bps, slash_bps)
    }

    /// Set the reputation NFT contract address.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `nft_contract` - Address of the reputation NFT contract
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn set_reputation_nft(env: Env, admin_signers: Vec<Address>, nft_contract: Address) {
        admin::set_reputation_nft(env, admin_signers, nft_contract)
    }

    /// Set the minimum stake amount per vouch.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `amount` - Minimum stake amount in stroops
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn set_min_stake(env: Env, admin_signers: Vec<Address>, amount: i128) {
        admin::set_min_stake(env, admin_signers, amount)
    }

    /// Set the maximum loan amount.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `amount` - Maximum loan amount in stroops (0 = no cap)
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn set_max_loan_amount(env: Env, admin_signers: Vec<Address>, amount: i128) {
        admin::set_max_loan_amount(env, admin_signers, amount)
    }

    /// Set the minimum number of vouchers required for a loan.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `count` - Minimum number of vouchers (0 = no minimum)
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn set_min_vouchers(env: Env, admin_signers: Vec<Address>, count: u32) {
        admin::set_min_vouchers(env, admin_signers, count)
    }

    /// Set the maximum loan-to-stake ratio.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `ratio` - Maximum ratio in basis points (e.g., 15000 = 150%)
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn set_max_loan_to_stake_ratio(env: Env, admin_signers: Vec<Address>, ratio: u32) {
        admin::set_max_loan_to_stake_ratio(env, admin_signers, ratio)
    }

    /// Add a token to the allowed tokens list.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `token` - Address of the token to add
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn set_max_vouchers_per_borrower(env: Env, admin_signers: Vec<Address>, max_vouchers: u32) {
        admin::set_max_vouchers_per_borrower(env, admin_signers, max_vouchers)
    }

    pub fn add_allowed_token(env: Env, admin_signers: Vec<Address>, token: Address) {
        admin::add_allowed_token(env, admin_signers, token)
    }

    /// Remove a token from the allowed tokens list.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `token` - Address of the token to remove
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn remove_allowed_token(env: Env, admin_signers: Vec<Address>, token: Address) {
        admin::remove_allowed_token(env, admin_signers, token)
    }

    // ── Governance ────────────────────────────────────────────────────────────

    /// Vote on a slash proposal for a borrower.
    ///
    /// # Arguments
    /// * `voucher` - Address of the voucher voting
    /// * `borrower` - Address of the borrower being voted on
    /// * `approve` - True to approve slash, false to reject
    ///
    /// # Panics
    /// * If voucher has not vouched for borrower
    /// * If voucher has already voted
    /// * If contract is paused
    pub fn vote_slash(
        env: Env,
        voucher: Address,
        borrower: Address,
        approve: bool,
    ) -> Result<(), ContractError> {
        governance::vote_slash(env, voucher, borrower, approve)
    }

    /// Set the slash vote quorum in basis points.
    ///
    /// # Arguments
    /// * `admin_signers` - Vector of admin addresses (must meet threshold)
    /// * `quorum_bps` - Quorum in basis points (e.g., 5000 = 50%)
    ///
    /// # Panics
    /// * If admin approval is insufficient
    pub fn set_slash_vote_quorum(env: Env, admin_signers: Vec<Address>, quorum_bps: u32) {
        helpers::require_admin_approval(&env, &admin_signers);
        governance::set_slash_vote_quorum(&env, quorum_bps);
    }

    /// Get the current slash vote quorum in basis points.
    ///
    /// # Returns
    /// * `u32` - The quorum in basis points
    pub fn get_slash_vote_quorum(env: Env) -> u32 {
        governance::get_slash_vote_quorum(env)
    }

    // ── Views ─────────────────────────────────────────────────────────────────

    /// Check if the contract has been initialized.
    ///
    /// # Returns
    /// * `bool` - True if initialized, false otherwise
    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&DataKey::Config)
    }

    /// Get the primary token address.
    ///
    /// # Returns
    /// * `Address` - The token address
    pub fn get_token(env: Env) -> Address {
        helpers::config(&env).token
    }

    /// Get the list of admin addresses.
    ///
    /// # Returns
    /// * `Vec<Address>` - Vector of admin addresses
    pub fn get_admins(env: Env) -> Vec<Address> {
        admin::get_admins(env)
    }

    /// Get the admin threshold (minimum number of admins required for approval).
    ///
    /// # Returns
    /// * `u32` - The admin threshold
    pub fn get_admin_threshold(env: Env) -> u32 {
        admin::get_admin_threshold(env)
    }

    /// Get the slash treasury balance.
    ///
    /// # Returns
    /// * `i128` - The slash treasury balance in stroops
    pub fn get_slash_treasury_balance(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::SlashTreasury)
            .unwrap_or(0)
    }

    /// Check if the contract is paused.
    ///
    /// # Returns
    /// * `bool` - True if paused, false otherwise
    pub fn get_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Get the loan status for a borrower.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `LoanStatus` - The current loan status
    pub fn loan_status(env: Env, borrower: Address) -> LoanStatus {
        loan::loan_status(env, borrower)
    }

    /// Check if a vouch exists for a borrower.
    ///
    /// # Arguments
    /// * `voucher` - Address of the voucher
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `bool` - True if vouch exists, false otherwise
    pub fn vouch_exists(env: Env, voucher: Address, borrower: Address) -> bool {
        vouch::vouch_exists(env, voucher, borrower)
    }

    /// Check if a voucher is whitelisted.
    ///
    /// # Arguments
    /// * `voucher` - Address of the voucher
    ///
    /// # Returns
    /// * `bool` - True if whitelisted, false otherwise
    pub fn is_whitelisted(env: Env, voucher: Address) -> bool {
        admin::is_whitelisted(env, voucher)
    }

    /// Get the loan record for a borrower.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `Option<LoanRecord>` - The loan record if exists, None otherwise
    pub fn get_loan(env: Env, borrower: Address) -> Option<LoanRecord> {
        loan::get_loan(env, borrower)
    }

    /// Get a loan record by ID.
    ///
    /// # Arguments
    /// * `loan_id` - The loan ID
    ///
    /// # Returns
    /// * `Option<LoanRecord>` - The loan record if exists, None otherwise
    pub fn get_loan_by_id(env: Env, loan_id: u64) -> Option<LoanRecord> {
        loan::get_loan_by_id(env, loan_id)
    }

    /// Get all vouches for a borrower.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `Option<Vec<VouchRecord>>` - Vector of vouch records if any exist, None otherwise
    pub fn get_vouches(env: Env, borrower: Address) -> Option<Vec<VouchRecord>> {
        env.storage().persistent().get(&DataKey::Vouches(borrower))
    }

    /// Check if a borrower is eligible for a loan.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    /// * `threshold` - Minimum total stake required
    /// * `token_addr` - Token address to filter vouches by
    ///
    /// # Returns
    /// * `bool` - True if eligible, false otherwise
    pub fn is_eligible(env: Env, borrower: Address, threshold: i128, token_addr: Address) -> bool {
        loan::is_eligible(env, borrower, threshold, token_addr)
    }

    /// Get the contract's token balance.
    ///
    /// # Returns
    /// * `i128` - The contract balance in stroops
    pub fn get_contract_balance(env: Env) -> i128 {
        helpers::token(&env).balance(&env.current_contract_address())
    }

    /// Get the voucher history (list of borrowers vouched for).
    ///
    /// # Arguments
    /// * `voucher` - Address of the voucher
    ///
    /// # Returns
    /// * `Vec<Address>` - Vector of borrower addresses
    pub fn voucher_history(env: Env, voucher: Address) -> Vec<Address> {
        vouch::voucher_history(env, voucher)
    }

    /// Get the reputation score for a borrower.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `u32` - The reputation score
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

    /// Get the total amount vouched for a borrower.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `Result<i128, ContractError>` - Total vouched amount or error if overflow
    pub fn total_vouched(env: Env, borrower: Address) -> Result<i128, ContractError> {
        vouch::total_vouched(env, borrower)
    }

    /// Get the repayment count for a borrower.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `u32` - The number of successful repayments
    pub fn repayment_count(env: Env, borrower: Address) -> u32 {
        loan::repayment_count(env, borrower)
    }

    /// Get the loan count for a borrower.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `u32` - The total number of historical loans
    pub fn loan_count(env: Env, borrower: Address) -> u32 {
        loan::loan_count(env, borrower)
    }

    /// Get the default count for a borrower.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `u32` - The total number of defaults
    pub fn default_count(env: Env, borrower: Address) -> u32 {
        loan::default_count(env, borrower)
    }

    /// Get the protocol fee in basis points.
    ///
    /// # Returns
    /// * `u32` - The protocol fee in basis points
    pub fn get_protocol_fee(env: Env) -> u32 {
        admin::get_protocol_fee(env)
    }

    /// Get the fee treasury address.
    ///
    /// # Returns
    /// * `Option<Address>` - The fee treasury address if set, None otherwise
    pub fn get_fee_treasury(env: Env) -> Option<Address> {
        admin::get_fee_treasury(env)
    }

    /// Check if a borrower is blacklisted.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `bool` - True if blacklisted, false otherwise
    pub fn is_blacklisted(env: Env, borrower: Address) -> bool {
        admin::is_blacklisted(env, borrower)
    }

    /// Get the minimum stake amount per vouch.
    ///
    /// # Returns
    /// * `i128` - The minimum stake amount in stroops
    pub fn get_min_stake(env: Env) -> i128 {
        admin::get_min_stake(env)
    }

    /// Get the maximum loan amount.
    ///
    /// # Returns
    /// * `i128` - The maximum loan amount in stroops (0 = no cap)
    pub fn get_max_loan_amount(env: Env) -> i128 {
        admin::get_max_loan_amount(env)
    }

    /// Get the minimum number of vouchers required for a loan.
    ///
    /// # Returns
    /// * `u32` - The minimum number of vouchers (0 = no minimum)
    pub fn get_min_vouchers(env: Env) -> u32 {
        admin::get_min_vouchers(env)
    }

    /// Get the maximum loan-to-stake ratio.
    ///
    /// # Returns
    /// * `u32` - The maximum ratio in basis points
    pub fn get_max_loan_to_stake_ratio(env: Env) -> u32 {
        admin::get_max_loan_to_stake_ratio(env)
    }

    /// Get the current protocol configuration.
    ///
    /// # Returns
    /// * `Config` - The configuration struct
    pub fn get_config(env: Env) -> Config {
        admin::get_config(env)
    }

    /// Propose a slash action with a confirmation window (timelock delay).
    ///
    /// Get the maximum number of vouchers per borrower.
    ///
    /// # Returns
    /// * `u32` - The maximum number of vouchers per borrower
    pub fn get_max_vouchers_per_borrower(env: Env) -> u32 {
        admin::get_max_vouchers_per_borrower(env)
    }

    /// Issue 109: Propose a slash action with a confirmation window (timelock delay).
    pub fn propose_slash(
        env: Env,
        proposer: Address,
        borrower: Address,
        delay_secs: u64,
    ) -> Result<u64, ContractError> {
        governance::propose_slash(env, proposer, borrower, delay_secs)
    }

    /// Issue 109: Execute a previously proposed slash after the delay has passed.
    pub fn execute_slash_proposal(env: Env, proposal_id: u64) -> Result<(), ContractError> {
        governance::execute_slash_proposal(env, proposal_id)
    }

    /// Cancel a pending slash proposal (only proposer can cancel).
    ///
    /// # Arguments
    /// * `caller` - Address of the caller (must be the proposer)
    /// * `proposal_id` - The proposal ID to cancel
    ///
    /// # Returns
    /// * `Result<(), ContractError>` - Success or error
    ///
    /// # Panics
    /// * If proposal does not exist
    /// * If caller is not the proposer
    /// * If proposal has already been executed or cancelled
    pub fn cancel_slash_proposal(
        env: Env,
        caller: Address,
        proposal_id: u64,
    ) -> Result<(), ContractError> {
        governance::cancel_slash_proposal(env, caller, proposal_id)
    }

    /// Get a timelock proposal details.
    ///
    /// # Arguments
    /// * `proposal_id` - The proposal ID
    ///
    /// # Returns
    /// * `Option<TimelockProposal>` - The proposal details if exists, None otherwise
    pub fn get_timelock_proposal(env: Env, proposal_id: u64) -> Option<TimelockProposal> {
        governance::get_timelock_proposal(env, proposal_id)
    }

    /// Get the slash vote record for a borrower.
    ///
    /// # Arguments
    /// * `borrower` - Address of the borrower
    ///
    /// # Returns
    /// * `Option<SlashVoteRecord>` - The slash vote record if exists, None otherwise
    pub fn get_slash_vote(env: Env, borrower: Address) -> Option<SlashVoteRecord> {
        governance::get_slash_vote(env, borrower)
    }
}
