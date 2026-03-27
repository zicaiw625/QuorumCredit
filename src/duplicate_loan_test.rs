/// Duplicate Loan Request Tests
///
/// Verifies that a second request_loan call while a loan is already active panics.
#[cfg(test)]
mod duplicate_loan_tests {
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

        StellarAssetClient::new(&env, &token_id.address()).mint(&contract_id, &10_000_000);

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&deployer, &admins, &1, &token_id.address());

        // Advance past MIN_VOUCH_AGE so vouches are eligible.
        env.ledger().with_mut(|l| l.timestamp = 120);

        Setup { env, client, token_id: token_id.address() }
    }

    fn do_vouch(s: &Setup, voucher: &Address, borrower: &Address, stake: i128) {
        StellarAssetClient::new(&s.env, &s.token_id).mint(voucher, &stake);
        s.client.vouch(voucher, borrower, &stake, &s.token_id);
    }

    fn do_loan(s: &Setup, borrower: &Address) {
        s.client.request_loan(
            borrower,
            &100_000,
            &500_000,
            &String::from_str(&s.env, "test"),
            &s.token_id,
        );
    }

    /// A second request_loan while a loan is active must panic with
    /// "borrower already has an active loan".
    #[test]
    #[should_panic(expected = "borrower already has an active loan")]
    fn test_request_loan_panics_when_loan_already_active() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher = Address::generate(&s.env);

        do_vouch(&s, &voucher, &borrower, 1_000_000);

        // First loan — succeeds.
        do_loan(&s, &borrower);

        // Second loan without repaying — must panic.
        do_loan(&s, &borrower);
    }

    /// Confirm the first loan is active before the duplicate attempt.
    #[test]
    fn test_active_loan_exists_after_first_request() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher = Address::generate(&s.env);

        do_vouch(&s, &voucher, &borrower, 1_000_000);
        do_loan(&s, &borrower);

        let loan = s.client.get_loan(&borrower).expect("loan should exist");
        assert!(!loan.repaid && !loan.defaulted, "loan should be active");

        // try_request_loan returns Err when a loan is already active.
        let result = s.client.try_request_loan(
            &borrower,
            &100_000,
            &500_000,
            &String::from_str(&s.env, "test"),
            &s.token_id,
        );
        assert!(result.is_err(), "second loan request must be rejected");
    }
}
