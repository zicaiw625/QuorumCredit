#[cfg(test)]
mod governance_tests {
    use crate::{ContractError, QuorumCreditContract, QuorumCreditContractClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::StellarAssetClient,
        Address, Env, Vec,
    };

    // ── Helpers ───────────────────────────────────────────────────────────────

    struct Setup {
        env: Env,
        client: QuorumCreditContractClient<'static>,
        admin: Address,
        token_id: Address,
    }

    fn setup() -> Setup {
        let env = Env::default();
        env.mock_all_auths();

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let admins = Vec::from_array(&env, [admin.clone()]);

        let token_id = env.register_stellar_asset_contract_v2(admin.clone());
        let contract_id = env.register_contract(None, QuorumCreditContract);

        // Fund the contract so it can disburse loans.
        StellarAssetClient::new(&env, &token_id.address()).mint(&contract_id, &10_000_000);

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&deployer, &admins, &1, &token_id.address());

        // Advance time past MIN_VOUCH_AGE (60s) so vouches are usable.
        env.ledger().with_mut(|l| l.timestamp = 120);

        Setup {
            env,
            client,
            admin,
            token_id: token_id.address(),
        }
    }

    /// Vouch for `borrower` from `voucher` with `stake`, minting tokens first.
    fn do_vouch(s: &Setup, voucher: &Address, borrower: &Address, stake: i128) {
        StellarAssetClient::new(&s.env, &s.token_id).mint(voucher, &stake);
        s.client.vouch(voucher, borrower, &stake, &s.token_id);
    }

    /// Request a loan for `borrower` (vouches must already meet threshold).
    fn do_loan(s: &Setup, borrower: &Address, amount: i128, threshold: i128) {
        s.client.request_loan(
            borrower,
            &amount,
            &threshold,
            &soroban_sdk::String::from_str(&s.env, "test loan"),
            &s.token_id,
        );
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// A single voucher holding >50% of stake approves → slash auto-executes.
    #[test]
    fn test_vote_slash_quorum_reached_executes_slash() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);

        // voucher_a has 600, voucher_b has 400 → total 1000
        do_vouch(&s, &voucher_a, &borrower, 600_000);
        do_vouch(&s, &voucher_b, &borrower, 400_000);
        do_loan(&s, &borrower, 100_000, 500_000);

        // voucher_a approves (60% > 50% default quorum) → slash fires immediately
        s.client.vote_slash(&voucher_a, &borrower, &true);

        // Loan must now be defaulted
        assert_eq!(
            s.client.loan_status(&borrower),
            crate::LoanStatus::Defaulted
        );

        // Slash treasury must hold 50% of voucher_a's stake (600_000 * 5000/10000 = 300_000)
        // plus 50% of voucher_b's stake (400_000 * 5000/10000 = 200_000) = 500_000
        assert_eq!(s.client.get_slash_treasury_balance(), 500_000);

        // Vote record must be marked executed
        let vote = s.client.get_slash_vote(&borrower).unwrap();
        assert!(vote.executed);
    }

    /// Two vouchers each hold 30% — neither alone reaches quorum; second vote tips it over.
    #[test]
    fn test_vote_slash_quorum_reached_on_second_vote() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);
        let voucher_c = Address::generate(&s.env);

        // a=300, b=300, c=400 → total 1000
        do_vouch(&s, &voucher_a, &borrower, 300_000);
        do_vouch(&s, &voucher_b, &borrower, 300_000);
        do_vouch(&s, &voucher_c, &borrower, 400_000);
        do_loan(&s, &borrower, 100_000, 500_000);

        // First vote: 30% — not enough
        s.client.vote_slash(&voucher_a, &borrower, &true);
        assert_eq!(s.client.loan_status(&borrower), crate::LoanStatus::Active);

        // Second vote: 30% + 30% = 60% ≥ 50% → slash fires
        s.client.vote_slash(&voucher_b, &borrower, &true);
        assert_eq!(
            s.client.loan_status(&borrower),
            crate::LoanStatus::Defaulted
        );
    }

    /// Voting against (reject) does not trigger slash even if approve stake is below quorum.
    #[test]
    fn test_vote_slash_reject_does_not_slash() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);

        do_vouch(&s, &voucher_a, &borrower, 600_000);
        do_loan(&s, &borrower, 100_000, 500_000);

        s.client.vote_slash(&voucher_a, &borrower, &false);

        // Loan still active
        assert_eq!(s.client.loan_status(&borrower), crate::LoanStatus::Active);
        let vote = s.client.get_slash_vote(&borrower).unwrap();
        assert!(!vote.executed);
        assert_eq!(vote.reject_stake, 600_000);
    }

    /// A voucher cannot vote twice on the same borrower.
    #[test]
    fn test_vote_slash_double_vote_rejected() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);

        do_vouch(&s, &voucher_a, &borrower, 300_000);
        do_vouch(&s, &voucher_b, &borrower, 700_000);
        do_loan(&s, &borrower, 100_000, 500_000);

        s.client.vote_slash(&voucher_b, &borrower, &false); // 70% reject — no slash

        let result = s.client.try_vote_slash(&voucher_b, &borrower, &true);
        assert_eq!(result, Err(Ok(ContractError::AlreadyVoted)));
    }

    /// A non-voucher cannot vote.
    #[test]
    fn test_vote_slash_non_voucher_rejected() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let outsider = Address::generate(&s.env);

        do_vouch(&s, &voucher_a, &borrower, 600_000);
        do_loan(&s, &borrower, 100_000, 500_000);

        let result = s.client.try_vote_slash(&outsider, &borrower, &true);
        assert_eq!(result, Err(Ok(ContractError::VoucherNotFound)));
    }

    /// Voting on a borrower with no active loan returns NoActiveLoan.
    #[test]
    fn test_vote_slash_no_active_loan_rejected() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);

        do_vouch(&s, &voucher_a, &borrower, 600_000);
        // No loan requested

        let result = s.client.try_vote_slash(&voucher_a, &borrower, &true);
        assert_eq!(result, Err(Ok(ContractError::NoActiveLoan)));
    }

    /// After slash executes, further votes return SlashAlreadyExecuted.
    #[test]
    fn test_vote_slash_after_execution_rejected() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);

        do_vouch(&s, &voucher_a, &borrower, 600_000);
        do_vouch(&s, &voucher_b, &borrower, 400_000);
        do_loan(&s, &borrower, 100_000, 500_000);

        // Slash executes on first vote (60% ≥ 50%)
        s.client.vote_slash(&voucher_a, &borrower, &true);

        let result = s.client.try_vote_slash(&voucher_b, &borrower, &true);
        assert_eq!(result, Err(Ok(ContractError::SlashAlreadyExecuted)));
    }

    /// Admin can change the quorum threshold; new threshold is respected.
    #[test]
    fn test_set_slash_vote_quorum_changes_threshold() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);

        // Raise quorum to 80%
        let admins = Vec::from_array(&s.env, [s.admin.clone()]);
        s.client.set_slash_vote_quorum(&admins, &8_000);
        assert_eq!(s.client.get_slash_vote_quorum(), 8_000);

        // a=600, b=400 → total 1000; 60% < 80% → no auto-slash on first vote
        do_vouch(&s, &voucher_a, &borrower, 600_000);
        do_vouch(&s, &voucher_b, &borrower, 400_000);
        do_loan(&s, &borrower, 100_000, 500_000);

        s.client.vote_slash(&voucher_a, &borrower, &true);
        assert_eq!(
            s.client.loan_status(&borrower),
            crate::LoanStatus::Active,
            "60% should not reach 80% quorum"
        );

        // Second vote: 60% + 40% = 100% ≥ 80% → slash fires
        s.client.vote_slash(&voucher_b, &borrower, &true);
        assert_eq!(
            s.client.loan_status(&borrower),
            crate::LoanStatus::Defaulted
        );
    }

    /// Issue #365: Mixed-token vouches — non-loan-token stakes are returned to vouchers.
    #[test]
    fn test_slash_mixed_token_vouches_returns_non_loan_token() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);

        // Create a second token
        let token2_id = s.env.register_stellar_asset_contract_v2(s.admin.clone());
        let token2 = token2_id.address();

        // Fund contract with both tokens
        StellarAssetClient::new(&s.env, &s.token_id).mint(&s.client.address, &10_000_000);
        StellarAssetClient::new(&s.env, &token2).mint(&s.client.address, &10_000_000);

        // voucher_a stakes in token1 (loan token)
        do_vouch(&s, &voucher_a, &borrower, 600_000);

        // voucher_b stakes in token2 (non-loan token)
        StellarAssetClient::new(&s.env, &token2).mint(&voucher_b, &400_000);
        s.client.vouch(&voucher_b, &borrower, &400_000, &token2);

        // Request loan in token1
        do_loan(&s, &borrower, 100_000, 500_000);

        // Slash via vote (voucher_a has 60% of token1 vouches)
        s.client.vote_slash(&voucher_a, &borrower, &true);

        // Loan must be defaulted
        assert_eq!(
            s.client.loan_status(&borrower),
            crate::LoanStatus::Defaulted
        );

        // voucher_a's remaining stake (50% of 600_000 = 300_000) should be returned
        let voucher_a_balance = StellarAssetClient::new(&s.env, &s.token_id).balance(&voucher_a);
        assert_eq!(voucher_a_balance, 300_000);

        // voucher_b's full stake (400_000) should be returned (non-loan token not slashed)
        let voucher_b_balance = StellarAssetClient::new(&s.env, &token2).balance(&voucher_b);
        assert_eq!(voucher_b_balance, 400_000);

        // Verify vouches are still stored (voucher_b's token2 vouch remains)
        let vouches = s.client.get_vouches(&borrower);
        assert_eq!(vouches.len(), 1);
        assert_eq!(vouches.get(0).unwrap().token, token2);
        assert_eq!(vouches.get(0).unwrap().stake, 400_000);
    }
}
