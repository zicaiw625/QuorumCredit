/// Multi-Voucher Stake Aggregation Tests
///
/// Verifies that multiple vouchers' partial stakes are correctly summed
/// to meet a loan threshold, and that a loan can be successfully requested
/// once the combined stake satisfies the threshold.
#[cfg(test)]
mod multi_voucher_stake_tests {
    use crate::{QuorumCreditContract, QuorumCreditContractClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::StellarAssetClient,
        Address, Env, String, Vec,
    };

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
        StellarAssetClient::new(&env, &token_id.address()).mint(&contract_id, &10_000_000);

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&deployer, &admins, &1, &token_id.address());

        // Advance time past MIN_VOUCH_AGE (60s) so vouches are eligible.
        env.ledger().with_mut(|l| l.timestamp = 120);

        Setup { env, client, token_id: token_id.address() }
    }

    fn do_vouch(s: &Setup, voucher: &Address, borrower: &Address, stake: i128) {
        StellarAssetClient::new(&s.env, &s.token_id).mint(voucher, &stake);
        s.client.vouch(voucher, borrower, &stake, &s.token_id);
    }

    fn purpose(env: &Env) -> String {
        String::from_str(env, "business expansion")
    }

    /// Three vouchers each contribute a partial stake. Their combined total
    /// must meet the threshold, and the loan request must succeed.
    ///
    /// Stakes:  voucher_a = 200_000
    ///          voucher_b = 150_000
    ///          voucher_c = 150_000
    ///          total     = 500_000  (== threshold)
    ///
    /// Loan amount = 100_000 (well within the 150% collateral ratio: 750_000 max).
    #[test]
    fn test_three_partial_stakes_sum_meets_threshold_and_loan_succeeds() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);
        let voucher_c = Address::generate(&s.env);

        // Each voucher contributes a partial stake — none alone meets the 500_000 threshold.
        do_vouch(&s, &voucher_a, &borrower, 200_000);
        do_vouch(&s, &voucher_b, &borrower, 150_000);
        do_vouch(&s, &voucher_c, &borrower, 150_000);

        // Verify the total vouched equals the sum of all three stakes.
        let total = s.client.total_vouched(&borrower).unwrap();
        assert_eq!(total, 500_000, "total_vouched should be 200_000 + 150_000 + 150_000");

        // Confirm the borrower is eligible at the 500_000 threshold.
        assert!(
            s.client.is_eligible(&borrower, &500_000, &s.token_id),
            "borrower should be eligible when combined stake meets threshold"
        );

        // Request the loan — should succeed because total stake >= threshold.
        s.client.request_loan(
            &borrower,
            &100_000,
            &500_000,
            &purpose(&s.env),
            &s.token_id,
        );

        // Confirm the loan is now active.
        let loan = s.client.get_loan(&borrower).expect("loan should exist after request");
        assert_eq!(loan.amount, 100_000);
        assert_eq!(loan.status, crate::LoanStatus::Active);
    }

    /// get_vouches on a fresh address with no vouches should return None (no entry).
    #[test]
    fn test_get_vouches_returns_none_for_address_with_no_vouches() {
        let s = setup();
        let fresh = Address::generate(&s.env);

        let result = s.client.get_vouches(&fresh);
        assert!(result.is_none(), "get_vouches should return None for an address with no vouches");
    }

    /// Verify that a loan request fails when the combined stake falls short of
    /// the threshold, even with multiple vouchers present.
    ///
    /// Stakes:  voucher_a = 100_000
    ///          voucher_b = 100_000
    ///          voucher_c = 100_000
    ///          total     = 300_000  (< 500_000 threshold)
    #[test]
    fn test_three_partial_stakes_below_threshold_rejects_loan() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher_a = Address::generate(&s.env);
        let voucher_b = Address::generate(&s.env);
        let voucher_c = Address::generate(&s.env);

        do_vouch(&s, &voucher_a, &borrower, 100_000);
        do_vouch(&s, &voucher_b, &borrower, 100_000);
        do_vouch(&s, &voucher_c, &borrower, 100_000);

        let total = s.client.total_vouched(&borrower).unwrap();
        assert_eq!(total, 300_000);

        // Loan request must fail — combined stake does not meet the threshold.
        let result = s.client.try_request_loan(
            &borrower,
            &100_000,
            &500_000,
            &purpose(&s.env),
            &s.token_id,
        );
        assert!(result.is_err(), "loan should be rejected when combined stake < threshold");
    }
}
