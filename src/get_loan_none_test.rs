/// get_loan None State Tests
///
/// Verifies that get_loan returns None for an address that has never requested a loan.
#[cfg(test)]
mod get_loan_none_tests {
    use crate::{QuorumCreditContract, QuorumCreditContractClient};
    use soroban_sdk::{testutils::Address as _, token::StellarAssetClient, Address, Env, Vec};

    fn setup() -> (Env, QuorumCreditContractClient<'static>) {
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

        (env, client)
    }

    /// Calling get_loan on a fresh address with no loan history should return None.
    ///
    /// Validates: Requirements 1.1, 1.2, 1.3
    #[test]
    fn test_get_loan_returns_none_for_address_with_no_loan() {
        let (env, client) = setup();
        let fresh = Address::generate(&env);

        let result = client.get_loan(&fresh);
        assert!(result.is_none(), "get_loan should return None for an address with no loan record");
    }

    /// Property 1: get_loan returns None for any address with no loan history
    ///
    /// Generates multiple fresh addresses and asserts each returns None from get_loan.
    ///
    /// **Validates: Requirements 1.2**
    #[test]
    fn test_get_loan_returns_none_for_multiple_fresh_addresses() {
        let (env, client) = setup();

        for _ in 0..20 {
            let fresh = Address::generate(&env);
            let result = client.get_loan(&fresh);
            assert!(
                result.is_none(),
                "get_loan should return None for any address with no loan history"
            );
        }
    }
}
