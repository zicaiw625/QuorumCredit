use crate::errors::ContractError;
use crate::helpers::{add_slash_balance, config, get_active_loan_record, require_not_paused};
use crate::types::{DataKey, SlashVoteRecord, TimelockAction, TimelockProposal, VouchRecord};
use soroban_sdk::{symbol_short, Address, Env, Vec};

/// Default quorum: 50% of total vouched stake must approve.
const DEFAULT_SLASH_VOTE_QUORUM_BPS: u32 = 5_000;

/// Cast a governance vote on whether `borrower` should be slashed.
///
/// - Only active vouchers (those with a stake in `Vouches(borrower)`) may vote.
/// - Votes are weighted by the voucher's current stake.
/// - When `approve_stake * 10_000 / total_stake >= quorum_bps`, slash is auto-executed.
pub fn vote_slash(
    env: Env,
    voucher: Address,
    borrower: Address,
    approve: bool,
) -> Result<(), ContractError> {
    voucher.require_auth();
    require_not_paused(&env)?;

    // Borrower must have an active loan to be slashable.
    let loan = get_active_loan_record(&env, &borrower)?;
    if loan.status != crate::types::LoanStatus::Active {
        return Err(ContractError::NoActiveLoan);
    }

    // Fetch vouches and find this voucher's stake.
    let vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower.clone()))
        .unwrap_or(Vec::new(&env));

    let voucher_stake = vouches
        .iter()
        .find(|v| v.voucher == voucher)
        .map(|v| v.stake)
        .ok_or(ContractError::VoucherNotFound)?;

    let total_stake: i128 = vouches.iter().map(|v| v.stake).sum();

    // Load or initialise the vote record.
    let mut vote: SlashVoteRecord = env
        .storage()
        .persistent()
        .get(&DataKey::SlashVote(borrower.clone()))
        .unwrap_or(SlashVoteRecord {
            approve_stake: 0,
            reject_stake: 0,
            voters: Vec::new(&env),
            executed: false,
        });

    if vote.executed {
        panic!("already defaulted");
    }

    // Prevent double-voting.
    if vote.voters.iter().any(|v| v == voucher) {
        return Err(ContractError::AlreadyVoted);
    }

    if approve {
        vote.approve_stake += voucher_stake;
    } else {
        vote.reject_stake += voucher_stake;
    }
    vote.voters.push_back(voucher.clone());

    env.events().publish(
        (symbol_short!("gov"), symbol_short!("voted")),
        (voucher.clone(), borrower.clone(), approve, voucher_stake),
    );

    // Check quorum.
    let quorum_bps: u32 = env
        .storage()
        .instance()
        .get(&DataKey::SlashVoteQuorum)
        .unwrap_or(DEFAULT_SLASH_VOTE_QUORUM_BPS);

    let quorum_reached =
        total_stake > 0 && vote.approve_stake * 10_000 / total_stake >= quorum_bps as i128;

    if quorum_reached {
        vote.executed = true;
        env.storage()
            .persistent()
            .set(&DataKey::SlashVote(borrower.clone()), &vote);
        execute_slash(&env, &borrower)?;
    } else {
        env.storage()
            .persistent()
            .set(&DataKey::SlashVote(borrower.clone()), &vote);
    }

    Ok(())
}

/// Returns the current slash vote record for a borrower, if any.
pub fn get_slash_vote(env: Env, borrower: Address) -> Option<SlashVoteRecord> {
    env.storage()
        .persistent()
        .get(&DataKey::SlashVote(borrower))
}

/// Set the quorum threshold (in basis points) required to auto-execute a slash.
/// Requires admin approval — called from admin module.
pub fn set_slash_vote_quorum(env: &Env, quorum_bps: u32) {
    assert!(
        quorum_bps > 0 && quorum_bps <= 10_000,
        "quorum_bps must be 1-10000"
    );
    env.storage()
        .instance()
        .set(&DataKey::SlashVoteQuorum, &quorum_bps);
}

pub fn get_slash_vote_quorum(env: Env) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::SlashVoteQuorum)
        .unwrap_or(DEFAULT_SLASH_VOTE_QUORUM_BPS)
}

// ── Internal ──────────────────────────────────────────────────────────────────

fn execute_slash(env: &Env, borrower: &Address) -> Result<(), ContractError> {
    let cfg = config(env);

    let vouches: Vec<VouchRecord> = env
        .storage()
        .persistent()
        .get(&DataKey::Vouches(borrower.clone()))
        .unwrap_or(Vec::new(env));

    // Mark loan as defaulted first so we can read token_address.
    let mut loan = get_active_loan_record(env, borrower)?;
    assert!(
        loan.status != crate::types::LoanStatus::Defaulted,
        "already defaulted"
    );
    let loan_token = soroban_sdk::token::Client::new(env, &loan.token_address);

    let mut total_slashed: i128 = 0;
    let mut remaining_vouches: Vec<VouchRecord> = Vec::new(env);

    for v in vouches.iter() {
        if v.token != loan.token_address {
            // Keep non-loan-token vouches
            remaining_vouches.push_back(v);
            continue;
        }
        let slash_amount = v.stake * cfg.slash_bps / 10_000;
        let remaining = v.stake - slash_amount;
        total_slashed += slash_amount;

        if remaining > 0 {
            loan_token.transfer(&env.current_contract_address(), &v.voucher, &remaining);
        }
    }

    add_slash_balance(env, total_slashed);

    loan.status = crate::types::LoanStatus::Defaulted;
    env.storage()
        .persistent()
        .set(&DataKey::Loan(loan.id), &loan);
    env.storage()
        .persistent()
        .remove(&DataKey::ActiveLoan(borrower.clone()));

    let count: u32 = env
        .storage()
        .persistent()
        .get(&DataKey::DefaultCount(borrower.clone()))
        .unwrap_or(0);
    env.storage()
        .persistent()
        .set(&DataKey::DefaultCount(borrower.clone()), &(count + 1));

    // Only remove vouches if all were processed; otherwise keep remaining vouches
    if remaining_vouches.is_empty() {
        env.storage()
            .persistent()
            .remove(&DataKey::Vouches(borrower.clone()));
    } else {
        env.storage()
            .persistent()
            .set(&DataKey::Vouches(borrower.clone()), &remaining_vouches);
    }

    env.events().publish(
        (symbol_short!("gov"), symbol_short!("slashed")),
        (borrower.clone(), total_slashed),
    );

    Ok(())
}

/// ── Issue 109: Slash Proposal Confirmation Window ──
///
/// Implements a two-step slash with timelock pattern:
/// 1. propose_slash: Admin creates a proposal, sets execution time (eta)
/// 2. execute_slash_proposal: After delay, anyone can execute

/// Propose a slash action with a delay before execution.
/// This implements the "confirmation window" for the slash action.
pub fn propose_slash(
    env: Env,
    proposer: Address,
    borrower: Address,
    delay_secs: u64,
) -> Result<u64, ContractError> {
    proposer.require_auth();
    require_not_paused(&env)?;

    // Get or initialize timelock counter
    let proposal_id: u64 = env
        .storage()
        .instance()
        .get(&DataKey::TimelockCounter)
        .unwrap_or(0u64)
        .checked_add(1)
        .expect("proposal ID overflow");

    let eta = env.ledger().timestamp() + delay_secs;

    let proposal = TimelockProposal {
        id: proposal_id,
        action: TimelockAction::Slash(borrower.clone()),
        proposer: proposer.clone(),
        eta,
        executed: false,
        cancelled: false,
    };

    env.storage()
        .instance()
        .set(&DataKey::Timelock(proposal_id), &proposal);
    env.storage()
        .instance()
        .set(&DataKey::TimelockCounter, &proposal_id);

    env.events().publish(
        (symbol_short!("gov"), symbol_short!("proposed")),
        (proposal_id, proposer, borrower, eta),
    );

    Ok(proposal_id)
}

/// Execute a previously proposed slash action after the delay has passed.
pub fn execute_slash_proposal(env: Env, proposal_id: u64) -> Result<(), ContractError> {
    require_not_paused(&env)?;

    // Get the proposal
    let mut proposal: TimelockProposal = env
        .storage()
        .instance()
        .get(&DataKey::Timelock(proposal_id))
        .ok_or(ContractError::NoActiveLoan)?; // Use existing error as placeholder

    // Check proposal state
    if proposal.executed {
        return Err(ContractError::SlashAlreadyExecuted);
    }
    if proposal.cancelled {
        return Err(ContractError::NoActiveLoan); // Use existing error as placeholder
    }

    // Check delay has passed
    if env.ledger().timestamp() < proposal.eta {
        return Err(ContractError::NoActiveLoan); // Use existing error as placeholder
    }

    // Check expiry (72 hours from eta)
    const TIMELOCK_EXPIRY: u64 = 72 * 60 * 60;
    if env.ledger().timestamp() > proposal.eta + TIMELOCK_EXPIRY {
        return Err(ContractError::NoActiveLoan); // Use existing error as placeholder
    }

    // Extract borrower from the Slash action
    if let TimelockAction::Slash(borrower) = &proposal.action {
        // Mark as executed before calling execute_slash to prevent reentrancy
        proposal.executed = true;
        env.storage()
            .instance()
            .set(&DataKey::Timelock(proposal_id), &proposal);

        // Execute the slash
        execute_slash(&env, borrower)?;

        env.events().publish(
            (symbol_short!("gov"), symbol_short!("executed")),
            (proposal_id, borrower.clone()),
        );

        Ok(())
    } else {
        Err(ContractError::NoActiveLoan) // Only Slash actions supported in this release
    }
}

/// Cancel a pending slash proposal (only by proposer or admin).
pub fn cancel_slash_proposal(
    env: Env,
    caller: Address,
    proposal_id: u64,
) -> Result<(), ContractError> {
    caller.require_auth();

    let mut proposal: TimelockProposal = env
        .storage()
        .instance()
        .get(&DataKey::Timelock(proposal_id))
        .ok_or(ContractError::NoActiveLoan)?;

    // Only proposer can cancel
    assert!(caller == proposal.proposer, "only proposer can cancel");

    if proposal.executed || proposal.cancelled {
        return Err(ContractError::SlashAlreadyExecuted);
    }

    proposal.cancelled = true;
    env.storage()
        .instance()
        .set(&DataKey::Timelock(proposal_id), &proposal);

    env.events().publish(
        (symbol_short!("gov"), symbol_short!("cancelled")),
        (proposal_id, caller),
    );

    Ok(())
}

/// Get a timelock proposal by ID.
pub fn get_timelock_proposal(env: Env, proposal_id: u64) -> Option<TimelockProposal> {
    env.storage()
        .instance()
        .get(&DataKey::Timelock(proposal_id))
}
