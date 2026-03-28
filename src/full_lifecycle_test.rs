#[cfg(test)]
mod full_lifecycle_tests {
    use crate::{QuorumCreditContract, QuorumCreditContractClient, LoanStatus};
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
    fn test_full_loan_lifecycle_initialize_vouch_request_repay() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let (deployer, admins, threshold, token) = setup(&env);

        // Step 1: Initialize contract
        client.initialize(&deployer, &admins, &threshold, &token);
        assert!(client.is_initialized(), "Contract should be initialized");

        // Step 2: Setup test accounts
        let borrower = Address::generate(&env);
        let voucher1 = Address::generate(&env);
        let voucher2 = Address::generate(&env);

        // Mint tokens to vouchers and borrower
        let token_client = soroban_sdk::token::Client::new(&env, &token);
        token_client.mint(&voucher1, &10_000);
        token_client.mint(&voucher2, &10_000);
        token_client.mint(&borrower, &1_000);

        // Step 3: Vouch for borrower
        client.vouch(&voucher1, &borrower, &5_000, &token);
        client.vouch(&voucher2, &borrower, &5_000, &token);

        // Verify vouches exist
        assert!(client.vouch_exists(&voucher1, &borrower), "Voucher1 vouch should exist");
        assert!(client.vouch_exists(&voucher2, &borrower), "Voucher2 vouch should exist");

        // Verify total vouched
        let total_vouched = client.total_vouched(&borrower);
        assert_eq!(total_vouched, 10_000, "Total vouched should be 10,000");

        // Step 4: Request loan
        let loan_amount = 5_000;
        let threshold = 5_000;
        let loan_purpose = soroban_sdk::String::from_str(&env, "Business expansion");
        client.request_loan(&borrower, &loan_amount, &threshold, &loan_purpose, &token);

        // Verify loan status is Active
        let loan_status = client.loan_status(&borrower);
        assert_eq!(loan_status, LoanStatus::Active, "Loan status should be Active");

        // Verify loan details
        let loan = client.get_loan(&borrower).expect("Loan should exist");
        assert_eq!(loan.amount, loan_amount, "Loan amount should match");
        assert_eq!(loan.status, LoanStatus::Active, "Loan status should be Active");
        assert_eq!(loan.borrower, borrower, "Borrower should match");

        // Verify borrower received the loan
        let borrower_balance = token_client.balance(&borrower);
        assert_eq!(borrower_balance, 1_000 + loan_amount, "Borrower should have received loan amount");

        // Step 5: Repay loan
        let repayment_amount = loan.amount + loan.total_yield;
        client.repay(&borrower, &repayment_amount);

        // Verify loan status is Repaid
        let loan_status = client.loan_status(&borrower);
        assert_eq!(loan_status, LoanStatus::Repaid, "Loan status should be Repaid");

        // Verify loan details after repayment
        let loan = client.get_loan(&borrower).expect("Loan should exist");
        assert_eq!(loan.status, LoanStatus::Repaid, "Loan status should be Repaid");
        assert!(loan.repayment_timestamp.is_some(), "Repayment timestamp should be set");

        // Verify vouchers received their stake back plus yield
        let voucher1_balance = token_client.balance(&voucher1);
        let voucher2_balance = token_client.balance(&voucher2);

        // Vouchers should have received their stake back plus yield
        // Initial balance: 10,000 - stake: 5,000 = 5,000
        // After repayment: 5,000 + 5,000 (stake) + yield
        assert!(voucher1_balance > 5_000, "Voucher1 should have received stake plus yield");
        assert!(voucher2_balance > 5_000, "Voucher2 should have received stake plus yield");

        // Verify vouches are cleared after repayment
        assert!(!client.vouch_exists(&voucher1, &borrower), "Voucher1 vouch should be cleared");
        assert!(!client.vouch_exists(&voucher2, &borrower), "Voucher2 vouch should be cleared");

        // Verify repayment count increased
        let repayment_count = client.repayment_count(&borrower);
        assert_eq!(repayment_count, 1, "Repayment count should be 1");

        // Verify loan count increased
        let loan_count = client.loan_count(&borrower);
        assert_eq!(loan_count, 1, "Loan count should be 1");
    }

    #[test]
    fn test_full_lifecycle_with_multiple_vouchers_and_partial_repayment() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let (deployer, admins, threshold, token) = setup(&env);

        // Initialize contract
        client.initialize(&deployer, &admins, &threshold, &token);

        // Setup test accounts
        let borrower = Address::generate(&env);
        let voucher1 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        let voucher3 = Address::generate(&env);

        // Mint tokens
        let token_client = soroban_sdk::token::Client::new(&env, &token);
        token_client.mint(&voucher1, &10_000);
        token_client.mint(&voucher2, &10_000);
        token_client.mint(&voucher3, &10_000);
        token_client.mint(&borrower, &1_000);

        // Vouch with different amounts
        client.vouch(&voucher1, &borrower, &3_000, &token);
        client.vouch(&voucher2, &borrower, &4_000, &token);
        client.vouch(&voucher3, &borrower, &3_000, &token);

        // Verify total vouched
        let total_vouched = client.total_vouched(&borrower);
        assert_eq!(total_vouched, 10_000, "Total vouched should be 10,000");

        // Request loan
        let loan_amount = 5_000;
        let threshold = 5_000;
        let loan_purpose = soroban_sdk::String::from_str(&env, "Working capital");
        client.request_loan(&borrower, &loan_amount, &threshold, &loan_purpose, &token);

        // Verify loan is active
        let loan_status = client.loan_status(&borrower);
        assert_eq!(loan_status, LoanStatus::Active, "Loan status should be Active");

        // Make partial repayment
        let partial_payment = 2_500;
        client.repay(&borrower, &partial_payment);

        // Verify loan is still active
        let loan_status = client.loan_status(&borrower);
        assert_eq!(loan_status, LoanStatus::Active, "Loan status should still be Active");

        // Verify amount repaid
        let loan = client.get_loan(&borrower).expect("Loan should exist");
        assert_eq!(loan.amount_repaid, partial_payment, "Amount repaid should match partial payment");

        // Make final repayment
        let remaining = loan.amount + loan.total_yield - partial_payment;
        client.repay(&borrower, &remaining);

        // Verify loan is repaid
        let loan_status = client.loan_status(&borrower);
        assert_eq!(loan_status, LoanStatus::Repaid, "Loan status should be Repaid");

        // Verify all vouchers received their stake back
        let voucher1_balance = token_client.balance(&voucher1);
        let voucher2_balance = token_client.balance(&voucher2);
        let voucher3_balance = token_client.balance(&voucher3);

        // Each voucher should have received their stake back plus yield
        assert!(voucher1_balance > 7_000, "Voucher1 should have received stake plus yield");
        assert!(voucher2_balance > 6_000, "Voucher2 should have received stake plus yield");
        assert!(voucher3_balance > 7_000, "Voucher3 should have received stake plus yield");
    }

    #[test]
    fn test_full_lifecycle_with_referral() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, QuorumCreditContract);
        let client = QuorumCreditContractClient::new(&env, &contract_id);

        let (deployer, admins, threshold, token) = setup(&env);

        // Initialize contract
        client.initialize(&deployer, &admins, &threshold, &token);

        // Setup test accounts
        let borrower = Address::generate(&env);
        let referrer = Address::generate(&env);
        let voucher = Address::generate(&env);

        // Mint tokens
        let token_client = soroban_sdk::token::Client::new(&env, &token);
        token_client.mint(&voucher, &10_000);
        token_client.mint(&borrower, &1_000);
        token_client.mint(&referrer, &1_000);

        // Register referral
        client.register_referral(&borrower, &referrer);

        // Verify referrer is set
        let stored_referrer = client.get_referrer(&borrower);
        assert_eq!(stored_referrer, Some(referrer.clone()), "Referrer should be set");

        // Vouch for borrower
        client.vouch(&voucher, &borrower, &10_000, &token);

        // Request loan
        let loan_amount = 5_000;
        let threshold = 5_000;
        let loan_purpose = soroban_sdk::String::from_str(&env, "Equipment purchase");
        client.request_loan(&borrower, &loan_amount, &threshold, &loan_purpose, &token);

        // Verify loan is active
        let loan_status = client.loan_status(&borrower);
        assert_eq!(loan_status, LoanStatus::Active, "Loan status should be Active");

        // Repay loan
        let loan = client.get_loan(&borrower).expect("Loan should exist");
        let repayment_amount = loan.amount + loan.total_yield;
        client.repay(&borrower, &repayment_amount);

        // Verify loan is repaid
        let loan_status = client.loan_status(&borrower);
        assert_eq!(loan_status, LoanStatus::Repaid, "Loan status should be Repaid");

        // Verify referrer received bonus
        let referrer_balance = token_client.balance(&referrer);
        assert!(referrer_balance > 1_000, "Referrer should have received bonus");
    }
}
