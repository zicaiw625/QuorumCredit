# Design Document: get-loan-none-test

## Overview

This feature adds a focused unit test that verifies `get_loan` returns `None` when called with an address that has never requested a loan from the `QuorumCreditContract`. The test confirms the contract's default/absent-record behavior and follows the established test file conventions in `QuorumCredit/src/`.

The implementation is minimal: one new test file (`get_loan_none_test.rs`) and one `#[cfg(test)]` module declaration added to `lib.rs`.

## Architecture

The test lives entirely within the existing Soroban test harness. No new modules, traits, or abstractions are introduced. The structure mirrors existing test files like `get_vouches_empty_test.rs`.

```mermaid
graph TD
    A[lib.rs] -->|#[cfg(test)] mod| B[get_loan_none_test.rs]
    B --> C[QuorumCreditContractClient]
    C --> D[get_loan(fresh_address)]
    D --> E{Option<LoanRecord>}
    E -->|None| F[assert!(result.is_none())]
```

## Components and Interfaces

### Test Module: `get_loan_none_test`

File: `QuorumCredit/src/get_loan_none_test.rs`

Follows the same structure as `get_vouches_empty_test.rs`:

- A `setup()` function that creates `Env::default()`, calls `mock_all_auths()`, registers the contract, mints tokens to the contract address, and initializes the contract via the client.
- A single `#[test]` function that generates a fresh `Address`, calls `client.get_loan(&fresh)`, and asserts the result `is_none()`.

### Contract Method Under Test

```rust
pub fn get_loan(env: Env, borrower: Address) -> Option<LoanRecord>
```

Defined in `loan.rs`. Delegates to `helpers::get_latest_loan_record`, which reads `DataKey::LatestLoan(borrower)` from persistent storage. For a fresh address this key is absent, so the function returns `None`.

### Module Declaration in `lib.rs`

Added alongside the existing `#[cfg(test)]` module declarations:

```rust
#[cfg(test)]
mod get_loan_none_test;
```

## Data Models

No new data models are introduced. The test exercises the existing `LoanRecord` type (via `Option<LoanRecord>`) and the `DataKey::LatestLoan` storage key, both defined in `types.rs`.

| Type | Source | Role in test |
|---|---|---|
| `LoanRecord` | `types.rs` | Return type of `get_loan`; expected to be absent (`None`) |
| `DataKey::LatestLoan(Address)` | `types.rs` | Storage key checked by `get_latest_loan_record`; absent for fresh address |
| `QuorumCreditContractClient` | SDK-generated | Invokes `get_loan` against the registered contract |

## Correctness Properties

*A property is a characteristic or behavior that should hold true across all valid executions of a system — essentially, a formal statement about what the system should do. Properties serve as the bridge between human-readable specifications and machine-verifiable correctness guarantees.*

### Property 1: get_loan returns None for any address with no loan history

*For any* address that has never called `request_loan` on the contract, calling `get_loan` with that address should return `None`.

**Validates: Requirements 1.2**

## Error Handling

The test does not exercise error paths. `get_loan` returns `Option<LoanRecord>` and never panics for a missing record — the absent storage key is handled by `unwrap_or`-style logic in `get_latest_loan_record`. The assertion `is_none()` is the only check needed.

## Testing Strategy

### Dual Testing Approach

**Unit test** (this feature): Verifies the specific example of a fresh address returning `None`. This is a concrete, deterministic example test — no randomization needed because the behavior is fully determined by the absence of any storage write.

**Property-based testing**: The correctness property above is amenable to property-based testing using the [`proptest`](https://github.com/proptest-rs/proptest) crate (or `quickcheck` for Rust). A property test would generate many random addresses, confirm none of them have loan records, and assert `get_loan` returns `None` for each.

Each property test should run a minimum of 100 iterations and be tagged with:

> Feature: get-loan-none-test, Property 1: get_loan returns None for any address with no loan history

### Unit Test Balance

- The single unit test covers the concrete example (Requirement 1.1–1.4).
- The property test covers the universal quantification (Requirement 1.2) across arbitrary addresses.
- No additional unit tests are needed; the behavior is fully captured by these two.
