#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, token, Address, BytesN, Env,
    Vec,
};

pub mod admin;
pub mod errors;
pub mod governance;
pub mod helpers;
pub mod loan;
pub mod reputation;
pub mod types;
pub mod vouch;

#[cfg(test)]
mod governance_test;
#[cfg(test)]
mod loan_purpose_test;
#[cfg(test)]
mod multi_asset_test;
#[cfg(test)]
mod referral_test;
#[cfg(test)]
mod security_fixes_test;
#[cfg(test)]
mod bug_condition_test;
#[cfg(test)]
mod slash_auth_test;

pub use errors::ContractError;
pub use types::*;

use helpers::{config, require_valid_token, validate_admin_config};
use reputation::ReputationNftExternalClient;

#[contract]
pub struct QuorumCreditContract;

#[contractimpl]
impl QuorumCreditContract {
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
                max_vouchers_per_loan: DEFAULT_MAX_VOUCHERS,
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

        // Validate numeric input: stake must be strictly positive.
        Self::require_positive_amount(&env, stake)?;

        assert!(voucher != borrower, "voucher cannot vouch for self");

        let cfg = Self::config(&env);

        // Sybil resistance: enforce minimum stake per vouch.
        let min_stake: i128 = env.storage().instance().get(&DataKey::MinStake).unwrap_or(0);
        if min_stake > 0 && stake < min_stake {
            return Err(ContractError::MinStakeNotMet);
        }

        // Enforce minimum yield stake: reject stakes that would produce zero yield
        // due to integer division truncation (stake * yield_bps / 10_000 == 0).
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
            .unwrap_or(Vec::new(&env));

        // Reject duplicate vouch before any state mutation or transfer.
        for v in vouches.iter() {
            if v.voucher == voucher {
                return Err(ContractError::DuplicateVouch);
            }
        }

        assert!(
            vouches.len() < cfg.max_vouchers_per_loan,
            "maximum vouchers per loan exceeded"
        );

        // Transfer stake from voucher into the contract.
        let token = Self::token_client(&env);
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
        let token = Self::token_client(&env);
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
            },
        );

        // Track borrower in the global list for admin pagination.
        let mut borrowers: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::BorrowerList)
            .unwrap_or(Vec::new(&env));
        borrowers.push_back(borrower.clone());
        env.storage()
            .persistent()
            .set(&DataKey::BorrowerList, &borrowers);

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

        let outstanding = loan.amount - loan.amount_repaid;
        assert!(
            payment > 0 && payment <= outstanding,
            "invalid payment amount"
        );

        let token = Self::token_client(&env);
        token.transfer(&borrower, &env.current_contract_address(), &payment);
        loan.amount_repaid += payment;

        let fully_repaid = loan.amount_repaid >= loan.amount;

        if fully_repaid {
            let cfg = Self::config(&env);
            let vouches: Vec<VouchRecord> = env
                .storage()
                .persistent()
                .get(&DataKey::Vouches(borrower.clone()))
                .unwrap_or(Vec::new(&env));

            let total_stake: i128 = vouches.iter().map(|v| v.stake).sum();
            let total_yield = loan.amount * cfg.yield_bps / 10_000;

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
                ReputationNftExternalClient::new(&env, &nft_addr).mint(&borrower);
            }

            env.events().publish(
                (symbol_short!("loan"), symbol_short!("repaid")),
                (borrower.clone(), loan.amount),
            );
        }

        // Persist the updated loan record (amount_repaid + repaid flag).
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

        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower));
    }

    /// Admin withdraws accumulated slashed funds to a recipient address.
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
            panic_with_error!(&env, ContractError::NoActiveLoan);
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

    /// Admin sets the maximum number of vouchers allowed per loan.
    /// Prevents unbounded voucher lists that could exhaust ledger gas in repay/slash loops.
    pub fn set_max_vouchers_per_loan(env: Env, admin_signers: Vec<Address>, max: u32) {
        Self::require_admin_approval(&env, &admin_signers);
        assert!(max > 0, "max_vouchers_per_loan must be greater than zero");
        let mut cfg = Self::config(&env);
        cfg.max_vouchers_per_loan = max;
        env.storage().instance().set(&DataKey::Config, &cfg);
    }

    /// Returns the current maximum vouchers per loan cap.
    pub fn get_max_vouchers_per_loan(env: Env) -> u32 {
        Self::config(&env).max_vouchers_per_loan
    }

    /// Admin updates configurable protocol parameters.
    pub fn set_config(env: Env, admin_signers: Vec<Address>, config: Config) {
        Self::require_admin_approval(&env, &admin_signers);
        assert!(config.yield_bps >= 0, "yield_bps must be non-negative");
        assert!(
            config.slash_bps > 0 && config.slash_bps <= 10_000,
            "slash_bps must be 1-10000"
        );
        assert!(config.max_vouchers_per_loan > 0, "max_vouchers_per_loan must be greater than zero");
        assert!(config.min_loan_amount > 0, "min_loan_amount must be greater than zero");
        assert!(config.loan_duration > 0, "loan_duration must be greater than zero");
        assert!(
            config.max_loan_to_stake_ratio > 0,
            "max_loan_to_stake_ratio must be greater than zero"
        );
        Self::validate_admin_config(&config.admins, config.admin_threshold);

        let old_config = Self::config(&env);
        if old_config.admins != config.admins {
            env.events().publish(
                (symbol_short!("admin"), symbol_short!("changed")),
                (old_config.admins.clone(), config.admins.clone()),
            );
        }

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

        if env.storage().instance().has(&DataKey::Config) {
            return Err(ContractError::AlreadyInitialized);
        }

        validate_admin_config(&env, &admins, admin_threshold)?;

        // Validate token address implements SEP-41 token interface before writing any state
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

    pub fn vouch(
        env: Env,
        voucher: Address,
        borrower: Address,
        stake: i128,
        token: Address,
    ) -> Result<(), ContractError> {
        vouch::vouch(env, voucher, borrower, stake, token)
    }

    pub fn batch_vouch(
        env: Env,
        voucher: Address,
        borrowers: Vec<Address>,
        stakes: Vec<i128>,
        token: Address,
    ) -> Result<(), ContractError> {
        vouch::batch_vouch(env, voucher, borrowers, stakes, token)
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

    pub fn register_referral(
        env: Env,
        borrower: Address,
        referrer: Address,
    ) -> Result<(), ContractError> {
        loan::register_referral(env, borrower, referrer)
    }

    pub fn get_referrer(env: Env, borrower: Address) -> Option<Address> {
        loan::get_referrer(env, borrower)
    }

    pub fn set_referral_bonus_bps(env: Env, admin_signers: Vec<Address>, bonus_bps: u32) {
        helpers::require_admin_approval(&env, &admin_signers);
        assert!(bonus_bps <= 10_000, "bonus_bps must not exceed 10000");
        env.storage()
            .instance()
            .set(&DataKey::ReferralBonusBps, &bonus_bps);
    }

    pub fn get_referral_bonus_bps(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ReferralBonusBps)
            .unwrap_or(crate::types::DEFAULT_REFERRAL_BONUS_BPS)
    }

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

    pub fn repay(env: Env, borrower: Address, payment: i128) -> Result<(), ContractError> {
        loan::repay(env, borrower, payment)
    }

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

    pub fn blacklist(env: Env, admin_signers: Vec<Address>, borrower: Address) {
        admin::blacklist(env, admin_signers, borrower)
    }

    pub fn set_config(env: Env, admin_signers: Vec<Address>, config: Config) {
        admin::set_config(env, admin_signers, config)
    }

    pub fn update_config(
        env: Env,
        admin_signers: Vec<Address>,
        yield_bps: Option<i128>,
        slash_bps: Option<i128>,
    ) {
        admin::update_config(env, admin_signers, yield_bps, slash_bps)
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

    pub fn total_vouched(env: Env, borrower: Address) -> Result<i128, ContractError> {
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
        admin::get_max_loan_to_stake_ratio(env)
    }

    pub fn get_config(env: Env) -> Config {
        admin::get_config(env)
    }

    pub fn add_allowed_token(env: Env, admin_signers: Vec<Address>, token: Address) {
        admin::add_allowed_token(env, admin_signers, token)
    }

    pub fn remove_allowed_token(env: Env, admin_signers: Vec<Address>, token: Address) {
        admin::remove_allowed_token(env, admin_signers, token)
    }

    pub fn vote_slash(
        env: Env,
        voucher: Address,
        borrower: Address,
        approve: bool,
    ) -> Result<(), ContractError> {
        governance::vote_slash(env, voucher, borrower, approve)
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
    pub fn execute_slash_proposal(
        env: Env,
        proposal_id: u64,
    ) -> Result<(), ContractError> {
        governance::execute_slash_proposal(env, proposal_id)
    }

    /// Issue 109: Cancel a pending slash proposal (only proposer can cancel).
    pub fn cancel_slash_proposal(
        env: Env,
        caller: Address,
        proposal_id: u64,
    ) -> Result<(), ContractError> {
        governance::cancel_slash_proposal(env, caller, proposal_id)
    }

    /// Issue 109: Get a timelock proposal details.
    pub fn get_timelock_proposal(env: Env, proposal_id: u64) -> Option<TimelockProposal> {
        governance::get_timelock_proposal(env, proposal_id)
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
        client.request_loan(&borrower, &Vec::new(&env), &500_000, &1_000_000);
        client.repay(&borrower, &500_000);
        assert_eq!(nft.balance(&borrower), 1);

        let borrower2 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        token_admin.mint(&voucher2, &2_000_000);

        nft.mint(&borrower2);
        assert_eq!(nft.balance(&borrower2), 1);

        client.vouch(&voucher2, &borrower2, &1_000_000);
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

    // ── Voucher Cap Tests ─────────────────────────────────────────────────────

    #[test]
    fn test_get_max_vouchers_per_loan_returns_default() {
        let env = Env::default();
        let (contract_id, _, _, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        assert_eq!(client.get_max_vouchers_per_loan(), DEFAULT_MAX_VOUCHERS);
    }

    #[test]
    fn test_set_max_vouchers_per_loan_and_get() {
        let env = Env::default();
        let (contract_id, _, admin, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);
        client.set_max_vouchers_per_loan(&admin_signers, &5);
        assert_eq!(client.get_max_vouchers_per_loan(), 5);
    }

    #[test]
    fn test_vouch_rejected_when_cap_reached() {
        let env = Env::default();
        let (contract_id, token_addr, admin, borrower, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let token_admin = StellarAssetClient::new(&env, &token_addr);
        let admin_signers = single_admin_signers(&env, &admin);

        client.set_max_vouchers_per_loan(&admin_signers, &2);

        for _ in 0..2 {
            let v = Address::generate(&env);
            token_admin.mint(&v, &1_000_000);
            client.vouch(&v, &borrower, &1_000_000);
        }

        let extra = Address::generate(&env);
        token_admin.mint(&extra, &1_000_000);
        // try_vouch returns Err on panic (host error), not a ContractError variant
        assert!(client.try_vouch(&extra, &borrower, &1_000_000).is_err());
    }

    #[test]
    #[should_panic(expected = "max_vouchers_per_loan must be greater than zero")]
    fn test_set_max_vouchers_per_loan_zero_rejected() {
        let env = Env::default();
        let (contract_id, _, admin, _, _) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);
        let admin_signers = single_admin_signers(&env, &admin);
        client.set_max_vouchers_per_loan(&admin_signers, &0);
    }

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

    pub fn set_slash_vote_quorum(env: Env, admin_signers: Vec<Address>, quorum_bps: u32) {
        helpers::require_admin_approval(&env, &admin_signers);
        governance::set_slash_vote_quorum(&env, quorum_bps);
    }

    pub fn get_slash_vote_quorum(env: Env) -> u32 {
        governance::get_slash_vote_quorum(env)
    }
}
