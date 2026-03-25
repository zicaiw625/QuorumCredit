#![allow(unused)]

use soroban_sdk::{contract, contractclient, contractimpl, contracttype, Address, Env};

#[contracttype]
pub enum RepKey {
    Minter,         // Address authorised to mint/burn (the main lending contract)
    Score(Address), // borrower → u32 reputation score
}

#[contractclient(name = "ReputationNftExternalClient")]
pub trait ReputationNftContractTrait {
    fn initialize(env: Env, minter: Address);
    fn mint(env: Env, to: Address);
    fn burn(env: Env, from: Address);
    fn balance(env: Env, addr: Address) -> u32;
}

#[cfg(test)]
#[contract]
pub struct ReputationNftContract;

#[cfg(test)]
#[contractimpl]
impl ReputationNftContract {
    /// One-time setup: record the authorised minter (the lending contract).
    pub fn initialize(env: Env, minter: Address) {
        assert!(
            !env.storage().instance().has(&RepKey::Minter),
            "already initialized"
        );
        env.storage().instance().set(&RepKey::Minter, &minter);
    }

    /// Mint one reputation point to `to`. Only callable by the registered minter.
    pub fn mint(env: Env, to: Address) {
        let minter: Address = env
            .storage()
            .instance()
            .get(&RepKey::Minter)
            .expect("not initialized");
        minter.require_auth();

        let score: u32 = env
            .storage()
            .persistent()
            .get(&RepKey::Score(to.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&RepKey::Score(to), &(score + 1));
    }

    /// Burn one reputation point from `from` (floor at 0). Only callable by the registered minter.
    pub fn burn(env: Env, from: Address) {
        let minter: Address = env
            .storage()
            .instance()
            .get(&RepKey::Minter)
            .expect("not initialized");
        minter.require_auth();

        let score: u32 = env
            .storage()
            .persistent()
            .get(&RepKey::Score(from.clone()))
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&RepKey::Score(from), &score.saturating_sub(1));
    }

    /// Returns the reputation score for `addr`.
    pub fn balance(env: Env, addr: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&RepKey::Score(addr))
            .unwrap_or(0)
    }
}
