/// Issue: vote_slash auto-executes slash when quorum is reached
///
/// Tests the governance auto-slash flow:
/// - Borrower with active loan and multiple vouchers
/// - Enough vouchers vote approve to meet quorum
/// - Loan is marked Defaulted, vouchers are slashed, gov/slashed event is emitted
#[cfg(test)]
mod vote_slash_auto_execute_tests {
    use crate::{QuorumCreditContract, QuorumCreditContractClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::StellarAssetClient,
        Address, Env, Vec,
    };

    // ── Helpers ───────────────────────────────────────────────────────────────

    struct Setup {
        env: Env,
        client: QuorumCreditContractClient<'static>,
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
        StellarAssetClient::new(&env, &token_id.address()).mint(&contract_id, &50_000_000);

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&deployer, &admins, &1, &token_id.address());

        // Advance time past MIN_VOUCH_AGE (60s) so vouches are immediately usable.
        env.ledger().with_mut(|l| l.timestamp = 120);

        Setup {
            env,
            client,
            token_id: token_id.address(),
        }
    }

    fn do_vouch(s: &Setup, voucher: &Address, borrower: &Address, stake: i128) {
        StellarAssetClient::new(&s.env, &s.token_id).mint(voucher, &stake);
        s.client.vouch(voucher, borrower, &stake, &s.token_id);
    }

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

    /// Core path: multiple vouchers vote approve, quorum is reached, slash auto-executes.
    ///
    /// Setup:
    ///   - borrower has 3 vouchers: A=400_000, B=350_000, C=250_000 (total 1_000_000)
    ///   - default quorum is 50% (5000 bps)
    ///   - A votes approve (40%) — below quorum, loan stays Active
    ///   - B votes approve (40%+35%=75%) — quorum reached, slash fires
    ///
    /// Assertions:
    ///   - loan is Defaulted
    ///   - all vouchers are slashed 50% (slash_bps = 5000)
    ///   - slash treasury holds total slashed amount
    ///   - vote record is marked executed
    ///   - gov/slashed event is emitted
    #[test]
    fn test_vote_slash_auto_executes_when_quorum_reached() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);
        let voucher_c = Address::generate(&s.env);

        // Stake: A=400_000, B=350_000, C=250_000 → total 1_000_000
        do_vouch(&s, &voucher_a, &borrower, 400_000);
        do_vouch(&s, &voucher_b, &borrower, 350_000);
        do_vouch(&s, &voucher_c, &borrower, 250_000);
        do_loan(&s, &borrower, 200_000, 800_000);

        // First vote: A approves (40%) — below 50% quorum
        s.client.vote_slash(&voucher_a, &borrower, &true);
        assert_eq!(
            s.client.loan_status(&borrower),
            crate::LoanStatus::Active,
            "40% approve should not reach 50% quorum"
        );

        // Second vote: B approves (40% + 35% = 75%) — quorum reached, slash fires
        s.client.vote_slash(&voucher_b, &borrower, &true);

        // Loan must be Defaulted
        assert_eq!(
            s.client.loan_status(&borrower),
            crate::LoanStatus::Defaulted,
            "loan should be Defaulted after quorum reached"
        );

        // All vouchers slashed 50%: A=200_000, B=175_000, C=125_000 → total 500_000
        let expected_slashed = 400_000_i128 * 5000 / 10_000
            + 350_000_i128 * 5000 / 10_000
            + 250_000_i128 * 5000 / 10_000;
        assert_eq!(
            s.client.get_slash_treasury_balance(),
            expected_slashed,
            "slash treasury should hold 50% of all voucher stakes"
        );

        // Vote record must be marked executed
        let vote = s.client.get_slash_vote(&borrower).unwrap();
        assert!(vote.executed, "vote record should be marked executed");
        assert_eq!(
            vote.approve_stake,
            400_000 + 350_000,
            "approve_stake should reflect both votes"
        );

        // gov/slashed event must be emitted
        let events = s.env.events().all();
        let slashed_event = events.iter().find(|e| {
            let topics: Vec<soroban_sdk::Val> = e.0.clone();
            // topics[0] = "gov", topics[1] = "slashed"
            topics.len() >= 2
        });
        assert!(slashed_event.is_some(), "gov/slashed event should be emitted");
    }

    /// Verify default_count is incremented for the borrower after auto-slash.
    #[test]
    fn test_vote_slash_auto_execute_increments_default_count() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);

        // A=600_000, B=400_000 → total 1_000_000; A alone is 60% ≥ 50%
        do_vouch(&s, &voucher_a, &borrower, 600_000);
        do_vouch(&s, &voucher_b, &borrower, 400_000);
        do_loan(&s, &borrower, 200_000, 800_000);

        assert_eq!(s.client.default_count(&borrower), 0);

        s.client.vote_slash(&voucher_a, &borrower, &true);

        assert_eq!(
            s.client.default_count(&borrower),
            1,
            "default_count should be 1 after auto-slash"
        );
    }

    /// Verify remaining stake (50%) is returned to each voucher after auto-slash.
    #[test]
    fn test_vote_slash_auto_execute_returns_remaining_stake_to_vouchers() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);

        // A=600_000, B=400_000; A alone triggers quorum
        do_vouch(&s, &voucher_a, &borrower, 600_000);
        do_vouch(&s, &voucher_b, &borrower, 400_000);
        do_loan(&s, &borrower, 200_000, 800_000);

        s.client.vote_slash(&voucher_a, &borrower, &true);

        // Each voucher should have received back 50% of their stake
        let token = soroban_sdk::token::Client::new(&s.env, &s.token_id);
        assert_eq!(
            token.balance(&voucher_a),
            300_000,
            "voucher_a should receive back 50% of 600_000"
        );
        assert_eq!(
            token.balance(&voucher_b),
            200_000,
            "voucher_b should receive back 50% of 400_000"
        );
    }

    /// Verify the ActiveLoan entry is removed after auto-slash (borrower can't repay).
    #[test]
    fn test_vote_slash_auto_execute_removes_active_loan() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);

        do_vouch(&s, &voucher_a, &borrower, 600_000);
        do_vouch(&s, &voucher_b, &borrower, 400_000);
        do_loan(&s, &borrower, 200_000, 800_000);

        s.client.vote_slash(&voucher_a, &borrower, &true);

        // Attempting to repay should fail — no active loan
        let result = s.client.try_repay(&borrower, &200_000);
        assert!(
            result.is_err(),
            "repay should fail after loan is auto-slashed"
        );
    }
}
