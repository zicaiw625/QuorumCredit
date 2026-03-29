#[cfg(test)]
mod vouch_cooldown_tests {
    use crate::{ContractError, QuorumCreditContract, QuorumCreditContractClient};
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

    /// Issue #366: Vouch cooldown is enforced — second vouch within cooldown window fails.
    #[test]
    fn test_vouch_cooldown_active_rejects_second_vouch() {
        let s = setup();
        let voucher = Address::generate(&s.env);
        let borrower1 = Address::generate(&s.env);
        let borrower2 = Address::generate(&s.env);

        // Mint tokens for voucher
        StellarAssetClient::new(&s.env, &s.token_id).mint(&voucher, &2_000_000);

        // First vouch succeeds
        s.client.vouch(&voucher, &borrower1, &1_000_000, &s.token_id);

        // Attempt second vouch immediately (within 24-hour cooldown) should fail
        let result = s.client.try_vouch(&voucher, &borrower2, &1_000_000, &s.token_id);
        assert_eq!(result, Err(Ok(ContractError::VouchCooldownActive)));
    }

    /// Issue #366: After cooldown expires, vouching is allowed again.
    #[test]
    fn test_vouch_cooldown_expires_allows_second_vouch() {
        let s = setup();
        let voucher = Address::generate(&s.env);
        let borrower1 = Address::generate(&s.env);
        let borrower2 = Address::generate(&s.env);

        // Mint tokens for voucher
        StellarAssetClient::new(&s.env, &s.token_id).mint(&voucher, &2_000_000);

        // First vouch at timestamp 120
        s.client.vouch(&voucher, &borrower1, &1_000_000, &s.token_id);

        // Advance time by 24 hours + 1 second (default cooldown is 24 hours)
        let cooldown_secs = 24 * 60 * 60;
        s.env.ledger().with_mut(|l| l.timestamp = 120 + cooldown_secs + 1);

        // Second vouch should now succeed
        s.client.vouch(&voucher, &borrower2, &1_000_000, &s.token_id);

        // Verify both vouches exist
        let vouches1 = s.client.get_vouches(&borrower1).unwrap();
        let vouches2 = s.client.get_vouches(&borrower2).unwrap();
        assert_eq!(vouches1.len(), 1);
        assert_eq!(vouches2.len(), 1);
    }

    /// Issue #366: Cooldown is per-voucher, not per-borrower.
    #[test]
    fn test_vouch_cooldown_is_per_voucher() {
        let s = setup();
        let voucher1 = Address::generate(&s.env);
        let voucher2 = Address::generate(&s.env);
        let borrower = Address::generate(&s.env);

        // Mint tokens
        StellarAssetClient::new(&s.env, &s.token_id).mint(&voucher1, &1_000_000);
        StellarAssetClient::new(&s.env, &s.token_id).mint(&voucher2, &1_000_000);

        // voucher1 vouches
        s.client.vouch(&voucher1, &borrower, &1_000_000, &s.token_id);

        // voucher2 can vouch immediately (different voucher, no cooldown)
        s.client.vouch(&voucher2, &borrower, &1_000_000, &s.token_id);

        // Verify both vouches exist
        let vouches = s.client.get_vouches(&borrower).unwrap();
        assert_eq!(vouches.len(), 2);
    }
}
