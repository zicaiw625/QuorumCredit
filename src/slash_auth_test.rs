/// Slash Authorization Tests
///
/// Verifies that slash panics when called by a non-admin address,
/// and succeeds when called by a legitimate admin.
#[cfg(test)]
mod slash_auth_tests {
    use crate::{QuorumCreditContract, QuorumCreditContractClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::StellarAssetClient,
        Address, Env, String, Vec,
    };

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

        StellarAssetClient::new(&env, &token_id.address()).mint(&contract_id, &10_000_000);

        let client = QuorumCreditContractClient::new(&env, &contract_id);
        client.initialize(&deployer, &admins, &1, &token_id.address());

        // Advance past MIN_VOUCH_AGE so vouches are eligible.
        env.ledger().with_mut(|l| l.timestamp = 120);

        Setup { env, client, admin, token_id: token_id.address() }
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

    /// Calling slash with a non-admin address must be rejected.
    /// require_admin_approval asserts the signer is a registered admin,
    /// so passing a random address causes a host panic.
    #[test]
    fn test_slash_panics_when_called_by_non_admin() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher = Address::generate(&s.env);
        let non_admin = Address::generate(&s.env);

        do_vouch(&s, &voucher, &borrower, 1_000_000);
        do_loan(&s, &borrower);

        // Pass the non-admin as the sole signer — must fail.
        let non_admin_signers = Vec::from_array(&s.env, [non_admin.clone()]);
        let result = s.client.try_slash(&non_admin_signers, &borrower);
        assert!(result.is_err(), "slash must be rejected when called by a non-admin address");
    }

    /// Calling slash with the registered admin must succeed.
    #[test]
    fn test_slash_succeeds_when_called_by_admin() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher = Address::generate(&s.env);

        do_vouch(&s, &voucher, &borrower, 1_000_000);
        do_loan(&s, &borrower);

        let admin_signers = Vec::from_array(&s.env, [s.admin.clone()]);
        s.client.slash(&admin_signers, &borrower);

        // Loan must now be defaulted.
        let loan = s.client.get_loan(&borrower).expect("loan should exist");
        assert!(loan.defaulted, "loan should be marked defaulted after slash");
    }
}
