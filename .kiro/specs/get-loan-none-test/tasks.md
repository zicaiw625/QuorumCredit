# Implementation Plan: get-loan-none-test

## Overview

Add `src/get_loan_none_test.rs` with a unit test verifying `get_loan` returns `None`
for a fresh address, then wire the module into `lib.rs`.

## Tasks

- [ ] 1. Create `src/get_loan_none_test.rs` test file
  - Mirror the setup pattern from `get_vouches_empty_test.rs`: `Env::default()`,
    `mock_all_auths()`, register contract, mint tokens, call `initialize`.
  - Add a single `#[test]` function that generates a fresh `Address` and asserts
    `client.get_loan(&fresh).is_none()`.
  - _Requirements: 1.1, 1.2, 1.3_

  - [ ]* 1.1 Write property test for get_loan None behavior
    - **Property 1: get_loan returns None for any address with no loan history**
    - Generate multiple random addresses, assert each returns `None` from `get_loan`.
    - **Validates: Requirements 1.2**

- [ ] 2. Wire module into `lib.rs`
  - Add `#[cfg(test)] mod get_loan_none_test;` alongside the existing test module
    declarations in `lib.rs`.
  - _Requirements: 1.4_

- [ ] 3. Checkpoint — Ensure all tests pass
  - Run `cargo test` and confirm the new test passes with no regressions.
    Ask the user if any questions arise.
