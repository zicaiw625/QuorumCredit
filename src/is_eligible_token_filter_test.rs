#[cfg(test)]
mod is_eligible_token_filter_tests {
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

    fn do_vouch(s: &Setup, voucher: &Address, borrower: &Address, stake: i128, token: &Address) {
        StellarAssetClient::new(&s.env, token).mint(voucher, &stake);
        s.client.vouch(voucher, borrower, &stake, token);
    }

    /// Issue #368: is_eligible filters vouches by token — USDC vouches don't count for XLM loans.
    #[test]
    fn test_is_eligible_filters_by_token() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher = Address::generate(&s.env);

        // Create a second token (USDC)
        let usdc_id = s.env.register_stellar_asset_contract_v2(s.admin.clone());
        let usdc = usdc_id.address();

        // Fund contract with both tokens
        StellarAssetClient::new(&s.env, &s.token_id).mint(&s.client.address, &10_000_000);
        StellarAssetClient::new(&s.env, &usdc).mint(&s.client.address, &10_000_000);

        // Vouch 1M in USDC (not the loan token)
        do_vouch(&s, &voucher, &borrower, 1_000_000, &usdc);

        // Borrower should NOT be eligible for XLM loan (threshold 500k)
        // because USDC vouches don't count
        assert!(
            !s.client.is_eligible(&borrower, &500_000, &s.token_id),
            "borrower should not be eligible for XLM loan with only USDC vouches"
        );

        // Borrower SHOULD be eligible for USDC loan
        assert!(
            s.client.is_eligible(&borrower, &500_000, &usdc),
            "borrower should be eligible for USDC loan with 1M USDC vouches"
        );
    }

    /// Issue #368: Mixed-token vouches — only matching token counts.
    #[test]
    fn test_is_eligible_mixed_tokens_counts_only_matching() {
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

        // voucher_a stakes 300k in token1
        do_vouch(&s, &voucher_a, &borrower, 300_000, &s.token_id);

        // voucher_b stakes 400k in token2
        do_vouch(&s, &voucher_b, &borrower, 400_000, &token2);

        // For token1 loan: only 300k counts (below 500k threshold)
        assert!(
            !s.client.is_eligible(&borrower, &500_000, &s.token_id),
            "token1 eligibility should only count token1 vouches"
        );

        // For token2 loan: only 400k counts (below 500k threshold)
        assert!(
            !s.client.is_eligible(&borrower, &500_000, &token2),
            "token2 eligibility should only count token2 vouches"
        );

        // For token1 loan with 300k threshold: eligible
        assert!(
            s.client.is_eligible(&borrower, &300_000, &s.token_id),
            "borrower should be eligible for token1 loan at 300k threshold"
        );

        // For token2 loan with 400k threshold: eligible
        assert!(
            s.client.is_eligible(&borrower, &400_000, &token2),
            "borrower should be eligible for token2 loan at 400k threshold"
        );
    }
}
