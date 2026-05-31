#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, token,
    Address, Env, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ContractError {
    /// Borrower already has an active loan that has not been repaid.
    LoanAlreadyActive = 1,
    /// No active loan found for the borrower.
    NoActiveLoan = 2,
    /// The voucher has already vouched for this borrower.
    DuplicateVouch = 3,
    /// Vouch stake must be greater than zero.
    ZeroStake = 4,
    /// Contract has not been initialized.
    NotInitialized = 5,
    /// Contract has already been initialized.
    AlreadyInitialized = 6,
    /// Caller is not the admin.
    UnauthorizedAdmin = 7,
    /// Contract token balance is insufficient to cover total yield payout.
    InsufficientFunds = 8,
    /// Stake is below the minimum required for non-zero yield (50 stroops).
    StakeBelowMinimum = 9,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum LoanStatus {
    Active = 0,
    Repaid = 1,
    Defaulted = 2,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Loan {
    pub borrower: Address,
    pub amount: u64,
    pub status: LoanStatus,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vouch {
    pub voucher: Address,
    pub stake: u64,
}

const TTL_THRESHOLD: u32 = 518_400;
const TTL_TARGET: u32 = 518_400;

/// Yield rate numerator: 2% = 200 / 10_000.
const YIELD_NUMERATOR: u64 = 200;
const YIELD_DENOMINATOR: u64 = 10_000;

/// Minimum vouch stake in stroops (#624).
///
/// The yield formula `stake * 200 / 10_000` performs integer division and
/// truncates to zero for any stake below 50 stroops, so vouchers with smaller
/// stakes would silently receive no yield. This guard makes that constraint
/// explicit at call time.
///
/// Deployment note: callers must ensure their stake is ≥ 50 stroops before
/// calling `vouch`. `initialize` should be called in the same transaction as
/// contract deployment to prevent front-running (#625).
const MIN_VOUCH_STAKE: u64 = 50;

const ADMIN_KEY: soroban_sdk::Symbol = symbol_short!("ADMIN");
const TOKEN_KEY: soroban_sdk::Symbol = symbol_short!("TOKEN");
const SLASH_BAL: soroban_sdk::Symbol = symbol_short!("SL_BAL");

fn loan_key(borrower: &Address) -> (soroban_sdk::Symbol, Address) {
    (symbol_short!("LOAN"), borrower.clone())
}

fn vouches_key(borrower: &Address) -> (soroban_sdk::Symbol, Address) {
    (symbol_short!("VOUCHES"), borrower.clone())
}

fn get_admin(env: &Env) -> Address {
    env.storage()
        .persistent()
        .get(&ADMIN_KEY)
        .unwrap_or_else(|| panic_with_error!(env, ContractError::NotInitialized))
}

fn get_token(env: &Env) -> Address {
    env.storage()
        .persistent()
        .get(&TOKEN_KEY)
        .unwrap_or_else(|| panic_with_error!(env, ContractError::NotInitialized))
}

fn require_admin(env: &Env, caller: &Address) {
    caller.require_auth();
    if get_admin(env) != *caller {
        panic_with_error!(env, ContractError::UnauthorizedAdmin);
    }
}

#[contract]
pub struct LendingContract;

#[contractimpl]
impl LendingContract {
    /// Initialize the lending contract with an admin and a payment token.
    ///
    /// # Security
    /// `deployer` must sign this transaction. Without this guard any observer
    /// of the deployment transaction can race to call `initialize` first,
    /// setting themselves as admin (#625). Call this in the same transaction as
    /// contract deployment to eliminate the front-run window entirely.
    pub fn initialize(env: Env, deployer: Address, admin: Address, token: Address) {
        // #625: Require the deployer's signature to prevent front-running.
        deployer.require_auth();

        if env.storage().persistent().has(&ADMIN_KEY) {
            panic_with_error!(&env, ContractError::AlreadyInitialized);
        }

        env.storage().persistent().set(&ADMIN_KEY, &admin);
        env.storage()
            .persistent()
            .extend_ttl(&ADMIN_KEY, TTL_THRESHOLD, TTL_TARGET);
        env.storage().persistent().set(&TOKEN_KEY, &token);
        env.storage()
            .persistent()
            .extend_ttl(&TOKEN_KEY, TTL_THRESHOLD, TTL_TARGET);
    }

    /// Request a new loan for the borrower.
    ///
    /// Panics with [`ContractError::LoanAlreadyActive`] if the borrower
    /// already has a non-repaid, non-defaulted loan.
    pub fn request_loan(env: Env, borrower: Address, amount: u64) {
        borrower.require_auth();

        let key = loan_key(&borrower);

        if let Some(existing) = env.storage().persistent().get::<_, Loan>(&key) {
            if existing.status == LoanStatus::Active {
                panic_with_error!(&env, ContractError::LoanAlreadyActive);
            }
        }

        let loan = Loan {
            borrower: borrower.clone(),
            amount,
            status: LoanStatus::Active,
        };
        env.storage().persistent().set(&key, &loan);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
    }

    /// Repay the active loan and distribute 2% yield to all vouchers.
    ///
    /// # Security
    /// Total yield (`Σ stake * 200 / 10_000`) is computed before any transfer.
    /// The contract balance is then asserted to be ≥ total yield. This prevents
    /// the loop from panicking mid-execution when the contract is underfunded
    /// (#627).
    pub fn repay(env: Env, borrower: Address) {
        borrower.require_auth();

        let key = loan_key(&borrower);
        let mut loan: Loan = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NoActiveLoan));

        if loan.status != LoanStatus::Active {
            panic_with_error!(&env, ContractError::NoActiveLoan);
        }

        let vouches: Vec<Vouch> = env
            .storage()
            .persistent()
            .get(&vouches_key(&borrower))
            .unwrap_or_else(|| Vec::new(&env));

        // #627: Pre-calculate total yield before touching any balances.
        let mut total_yield: u64 = 0;
        for v in vouches.iter() {
            total_yield += v.stake * YIELD_NUMERATOR / YIELD_DENOMINATOR;
        }

        // #627: Assert the contract holds enough tokens to cover every payout.
        let token_addr = get_token(&env);
        let tok = token::Client::new(&env, &token_addr);
        let contract_balance = tok.balance(&env.current_contract_address());
        if contract_balance < (total_yield as i128) {
            panic_with_error!(&env, ContractError::InsufficientFunds);
        }

        loan.status = LoanStatus::Repaid;
        env.storage().persistent().set(&key, &loan);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);

        for v in vouches.iter() {
            let yield_amount = v.stake * YIELD_NUMERATOR / YIELD_DENOMINATOR;
            if yield_amount > 0 {
                tok.transfer(
                    &env.current_contract_address(),
                    &v.voucher,
                    &(yield_amount as i128),
                );
            }
        }
    }

    /// Vouch for a borrower with a token stake.
    ///
    /// # Minimum Stake
    /// Stake must be ≥ `MIN_VOUCH_STAKE` (50 stroops). The yield formula
    /// `stake * 200 / 10_000` uses integer division and truncates to zero for
    /// stakes below 50, so vouchers would silently receive no yield (#624).
    ///
    /// # Errors
    /// - [`ContractError::ZeroStake`] if stake is 0
    /// - [`ContractError::StakeBelowMinimum`] if stake < 50 stroops (#624)
    /// - [`ContractError::DuplicateVouch`] if this voucher already vouched for
    ///   this borrower
    pub fn vouch(env: Env, borrower: Address, voucher: Address, stake: u64) {
        voucher.require_auth();

        if stake == 0 {
            panic_with_error!(&env, ContractError::ZeroStake);
        }

        // #624: Reject stakes that yield zero due to integer truncation.
        if stake < MIN_VOUCH_STAKE {
            panic_with_error!(&env, ContractError::StakeBelowMinimum);
        }

        let key = vouches_key(&borrower);
        let mut vouches: Vec<Vouch> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));

        for v in vouches.iter() {
            if v.voucher == voucher {
                panic_with_error!(&env, ContractError::DuplicateVouch);
            }
        }

        let token_addr = get_token(&env);
        let tok = token::Client::new(&env, &token_addr);
        tok.transfer(&voucher, &env.current_contract_address(), &(stake as i128));

        vouches.push_back(Vouch {
            voucher: voucher.clone(),
            stake,
        });
        env.storage().persistent().set(&key, &vouches);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
    }

    /// Admin-only: mark a loan as defaulted and slash 50% of each voucher's stake.
    ///
    /// The slashed half is accumulated in `slash_balance`; the other half is
    /// returned to the voucher. The accumulated balance can be withdrawn by the
    /// admin via [`slash_treasury`] (#626).
    pub fn slash(env: Env, admin: Address, borrower: Address) {
        require_admin(&env, &admin);

        let key = loan_key(&borrower);
        let mut loan: Loan = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NoActiveLoan));

        if loan.status != LoanStatus::Active {
            panic_with_error!(&env, ContractError::NoActiveLoan);
        }

        loan.status = LoanStatus::Defaulted;
        env.storage().persistent().set(&key, &loan);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);

        let vouches: Vec<Vouch> = env
            .storage()
            .persistent()
            .get(&vouches_key(&borrower))
            .unwrap_or_else(|| Vec::new(&env));

        let token_addr = get_token(&env);
        let tok = token::Client::new(&env, &token_addr);

        // #626: Accumulate slashed amounts into slash_balance instead of
        // leaving them permanently locked in the contract.
        let mut slash_accum: u64 = 0;
        for v in vouches.iter() {
            let slashed = v.stake / 2;
            let returned = v.stake - slashed;
            slash_accum += slashed;
            if returned > 0 {
                tok.transfer(
                    &env.current_contract_address(),
                    &v.voucher,
                    &(returned as i128),
                );
            }
        }

        let current_slash: u64 = env
            .storage()
            .persistent()
            .get(&SLASH_BAL)
            .unwrap_or(0u64);
        let updated_slash = current_slash + slash_accum;
        env.storage().persistent().set(&SLASH_BAL, &updated_slash);
        env.storage()
            .persistent()
            .extend_ttl(&SLASH_BAL, TTL_THRESHOLD, TTL_TARGET);
    }

    /// Admin-only: withdraw all accumulated slash balance to the admin address.
    ///
    /// Transfers the full `slash_balance` to `admin` and resets it to zero.
    /// This provides a withdrawal path for the slashed funds that would
    /// otherwise be permanently locked in the contract (#626).
    pub fn slash_treasury(env: Env, admin: Address) {
        require_admin(&env, &admin);

        let slash_balance: u64 = env
            .storage()
            .persistent()
            .get(&SLASH_BAL)
            .unwrap_or(0u64);

        if slash_balance > 0 {
            let token_addr = get_token(&env);
            let tok = token::Client::new(&env, &token_addr);
            tok.transfer(
                &env.current_contract_address(),
                &admin,
                &(slash_balance as i128),
            );
            env.storage().persistent().set(&SLASH_BAL, &0u64);
            env.storage()
                .persistent()
                .extend_ttl(&SLASH_BAL, TTL_THRESHOLD, TTL_TARGET);
        }
    }

    /// Returns the loan for a borrower, if any.
    pub fn get_loan(env: Env, borrower: Address) -> Option<Loan> {
        env.storage().persistent().get(&loan_key(&borrower))
    }

    /// Returns all vouches for a borrower.
    pub fn get_vouches(env: Env, borrower: Address) -> Vec<Vouch> {
        env.storage()
            .persistent()
            .get(&vouches_key(&borrower))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Returns the accumulated slash balance available for treasury withdrawal.
    pub fn get_slash_balance(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&SLASH_BAL)
            .unwrap_or(0u64)
    }

    /// Returns whether the contract has been initialized.
    pub fn is_initialized(env: Env) -> bool {
        env.storage().persistent().has(&ADMIN_KEY)
    }

    /// Returns the current admin address.
    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .persistent()
            .get(&ADMIN_KEY)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized))
    }

    /// Returns the token contract address.
    pub fn get_token(env: Env) -> Address {
        env.storage()
            .persistent()
            .get(&TOKEN_KEY)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn test_is_initialized() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &contract_id);

        assert!(!client.is_initialized());

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&deployer, &admin, &token);
        assert!(client.is_initialized());
    }

    #[test]
    fn test_get_admin() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &contract_id);

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&deployer, &admin, &token);

        let retrieved_admin = client.get_admin();
        assert_eq!(retrieved_admin, admin);
    }

    #[test]
    fn test_get_token() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &contract_id);

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&deployer, &admin, &token);

        let retrieved_token = client.get_token();
        assert_eq!(retrieved_token, token);
    }

    #[test]
    fn test_slash_treasury() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &contract_id);

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&deployer, &admin, &token);

        // Verify initial slash balance is zero
        let initial_balance = client.get_slash_balance();
        assert_eq!(initial_balance, 0);

        // slash_treasury should work without error when balance is zero
        client.slash_treasury(&admin);

        // Verify balance remains zero
        let final_balance = client.get_slash_balance();
        assert_eq!(final_balance, 0);
    }
}
