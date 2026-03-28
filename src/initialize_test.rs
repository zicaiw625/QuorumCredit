#[cfg(test)]
mod initialize_tests {
    use crate::{QuorumCreditContract, QuorumCreditContractClient};
    use soroban_sdk::{testutils::Address as _, Address, Env, Vec};

    fn setup(env: &Env) -> (Address, Vec<Address>, u32, Address) {
        let deployer = Address::generate(env);
        let admin = Address::generate(env);
        let admins = Vec::from_array(env, [admin]);
        let token = env
            .register_stellar_asset_contract_v2(Address::generate(env))
            .address();
        (deployer, admins, 1, token)
    }

    #[test]
    #[should_panic]
    fn test_double_initialize_panics_with_already_initialized() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let (deployer, admins, threshold, token) = setup(&env);

        client.initialize(&deployer, &admins, &threshold, &token);
        // Second call must panic with AlreadyInitialized (error code 19)
        client.initialize(&deployer, &admins, &threshold, &token);
    }

    #[test]
    #[should_panic(expected = "ZeroAddress")]
    fn test_initialize_with_zero_address_admin_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let deployer = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();

        // Create zero address (all-zeros account)
        let zero_address = Address::from_string(&soroban_sdk::String::from_str(
            &env,
            "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
        ));

        let admins = Vec::from_array(&env, [zero_address]);
        let threshold = 1;

        // Should panic with ZeroAddress error
        client.initialize(&deployer, &admins, &threshold, &token);
    }
}
