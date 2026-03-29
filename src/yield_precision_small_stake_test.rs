/// Yield Calculation Precision Test - Small Stakes (< 50 stroops)
///
/// Tests truncation behavior: yield_bps=200 (2%), stake S gives (S * 200) / 10000 = S / 50.
/// S=49 truncates to 0 yield.
///
/// 1. Vouch 49 stroops
/// 2. Request loan amount=10, threshold=49
/// 3. Repay 10 (principal only; expected_yield=10*200/10000=0)
/// 4. Assert voucher balance unchanged (stake returned + 0 yield)

#[cfg(test)]
mod yield_precision_small_stake_tests {
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

        StellarAssetClient::new(&env, &token_id.address()).mint(&contract_id, &100_000);

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

    /// Test core issue: stake=49 stroops (<50) produces 0 yield due to truncation.
    ///
    /// Yield math: loan=10, yield_bps=200 → total_yield = 10 * 200 / 10000 = 0
    /// Even if total_yield >0, voucher_yield = (total_yield * 49) / 49 = total_yield truncates if small.
    #[test]
    fn test_vouch_49_strops_truncates_yield_to_zero() {
        let s = setup();
        let borrower = Address::generate(&s.env);
        let voucher = Address::generate(&s.env);
        let stake = 49i128;
        let loan_amount = 10i128;  // total_yield = 10 * 200 / 10000 = 0 truncates
        let threshold = 49i128;    // Within max_loan_to_stake_ratio=150% (73 max)

        // Initial voucher balance after minting exactly stake amount.
        StellarAssetClient::new(&s.env, &s.token_id).mint(&voucher, &stake);
        let initial_balance = StellarAssetClient::new(&s.env, &s.token_id).balance(&voucher);
        assert_eq!(initial_balance, stake, "voucher should have exactly stake amount");

        // Vouch with small stake.
        do_vouch(&s, &voucher, &borrower, stake);

        // Verify total vouched stake recorded correctly.
        let total_vouched = s.client.total_vouched(&borrower).unwrap();
        assert_eq!(total_vouched, stake, "total_vouched should equal vouch stake");

        // Borrower eligible since stake == threshold.
        assert!(s.client.is_eligible(&borrower, &threshold, &s.token_id));

        // Request small loan (succeeds as threshold met).
        s.client.request_loan(
            &borrower,
            &loan_amount,
            &threshold,
            &String::from_str(&s.env, "test purpose"),
            &s.token_id,
        );

        // Verify loan created with correct total_yield (truncates to 0).
        let loan = s.client.get_loan(&borrower).expect("loan should exist");
        let expected_yield = loan_amount * 200 / 10_000;  // 0
        assert_eq!(loan.total_yield, expected_yield, "total_yield should truncate to 0");
        assert_eq!(loan.amount, loan_amount);

        // Repay exactly principal (no yield owed).
        s.client.repay(&borrower, &loan_amount);

        // Loan fully repaid.
        let repaid_loan = s.client.get_loan(&borrower).expect("loan should exist after repay");
        assert_eq!(repaid_loan.status, crate::LoanStatus::Repaid, "loan should be marked repaid");

        // CRITICAL: Voucher receives stake back + 0 yield due to truncation.
        // Final balance == initial_balance (stake returned exactly).
        let final_balance = StellarAssetClient::new(&s.env, &s.token_id).balance(&voucher);
        assert_eq!(
            final_balance, initial_balance,
            "voucher should receive stake back + 0 yield (truncation); got {} expected {}",
            final_balance, initial_balance
        );

        // Vouches cleared after repayment.
        assert!(s.client.get_vouches(&borrower).is_none());
    }
}

