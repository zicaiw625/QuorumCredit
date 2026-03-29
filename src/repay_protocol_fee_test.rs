#[cfg(test)]
mod repay_protocol_fee_tests {
    use crate::{QuorumCreditContract, QuorumCreditContractClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::StellarAssetClient,
        Address, Env, Vec,
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

        env.ledger().with_mut(|l| l.timestamp = 120);

        Setup {
            env,
            client,
            admin,
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

    /// Issue #367: Protocol fee is collected and transferred to fee treasury on repayment.
    #[test]
    fn test_repay_collects_protocol_fee() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher = Address::generate(&s.env);
        let fee_treasury = Address::generate(&s.env);

        // Set protocol fee to 5% (500 bps)
        let admins = Vec::from_array(&s.env, [s.admin.clone()]);
        s.client.set_protocol_fee(&admins, &500);
        s.client.set_fee_treasury(&admins, &fee_treasury);

        // Setup loan
        do_vouch(&s, &voucher, &borrower, 1_000_000);
        do_loan(&s, &borrower, 100_000, 500_000);

        // Mint repayment funds for borrower
        StellarAssetClient::new(&s.env, &s.token_id).mint(&borrower, &102_000);

        // Repay full loan (principal + yield)
        s.client.repay(&borrower, &102_000);

        // Fee treasury should receive 5% of loan amount (100_000 * 500 / 10_000 = 5_000)
        let fee_treasury_balance = StellarAssetClient::new(&s.env, &s.token_id).balance(&fee_treasury);
        assert_eq!(fee_treasury_balance, 5_000);

        // Voucher should receive stake + yield (minus fee impact)
        let voucher_balance = StellarAssetClient::new(&s.env, &s.token_id).balance(&voucher);
        // Yield is 2% of 100_000 = 2_000
        // Voucher gets: 1_000_000 (stake) + 2_000 (yield) = 1_002_000
        assert_eq!(voucher_balance, 1_002_000);
    }

    /// Issue #367: Protocol fee is zero when not configured.
    #[test]
    fn test_repay_no_fee_when_not_configured() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher = Address::generate(&s.env);
        let fee_treasury = Address::generate(&s.env);

        // Fee treasury set but protocol fee is 0 (default)
        let admins = Vec::from_array(&s.env, [s.admin.clone()]);
        s.client.set_fee_treasury(&admins, &fee_treasury);

        // Setup loan
        do_vouch(&s, &voucher, &borrower, 1_000_000);
        do_loan(&s, &borrower, 100_000, 500_000);

        // Mint repayment funds for borrower
        StellarAssetClient::new(&s.env, &s.token_id).mint(&borrower, &102_000);

        // Repay full loan
        s.client.repay(&borrower, &102_000);

        // Fee treasury should receive nothing (fee is 0)
        let fee_treasury_balance = StellarAssetClient::new(&s.env, &s.token_id).balance(&fee_treasury);
        assert_eq!(fee_treasury_balance, 0);

        // Voucher should receive full stake + yield
        let voucher_balance = StellarAssetClient::new(&s.env, &s.token_id).balance(&voucher);
        assert_eq!(voucher_balance, 1_002_000);
    }
}
