#[cfg(test)]
mod request_loan_insufficient_stake_tests {
    use crate::errors::ContractError;
    use crate::{QuorumCreditContract, QuorumCreditContractClient};
    use soroban_sdk::{testutils::Address as _, token::StellarAssetClient, Address, Env, String, Vec};

    fn setup(env: &Env) -> (Address, Address, Address, Address) {
        let deployer = Address::generate(env);
        let admin = Address::generate(env);
        let token_id = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(env, &contract_id);
        client.initialize(&deployer, &Vec::from_array(env, [admin]), &1, &token_id);
        StellarAssetClient::new(env, &token_id).mint(&contract_id, &10_000_000);
        let voucher = Address::generate(env);
        StellarAssetClient::new(env, &token_id).mint(&voucher, &1_000_000);
        (contract_id, token_id, voucher, Address::generate(env))
    }

    #[test]
    fn test_request_loan_below_threshold_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 120);
        let (contract_id, token_id, voucher, borrower) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        // Vouch with 100_000 stake but request loan with threshold of 500_000
        client.vouch(&voucher, &borrower, &100_000, &token_id);

        let result = client.try_request_loan(
            &borrower,
            &100_000,
            &500_000,
            &String::from_str(&env, "test"),
            &token_id,
        );
        assert_eq!(result, Err(Ok(ContractError::InsufficientFunds)));
    }

    #[test]
    #[should_panic]
    fn test_request_loan_zero_threshold_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 120);
        let (contract_id, token_id, voucher, borrower) = setup(&env);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        client.vouch(&voucher, &borrower, &100_000, &token_id);

        // threshold = 0 must be rejected
        client.request_loan(
            &borrower,
            &100_000,
            &0,
            &String::from_str(&env, "test"),
            &token_id,
        );
    }
}
