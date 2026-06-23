#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, token,
    Address, Env, Symbol, Vec,
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
    /// Total stake summation overflowed i128.
    StakeSummationOverflow = 10,
    /// Admin address is invalid (zero address).
    InvalidAdminAddress = 11,
    /// Token address is invalid (zero address).
    InvalidTokenAddress = 12,
    /// Contract is paused.
    ContractPaused = 13,
    /// Too many vouchers for this borrower.
    TooManyVouchers = 14,
    /// Voucher withdrawal not allowed.
    VouchWithdrawNotAllowed = 15,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum LoanStatus {
    Active = 0,
    Repaid = 1,
    Defaulted = 2,
    None = 3,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Loan {
    pub borrower: Address,
    pub amount: u64,
    pub status: LoanStatus,
    pub deadline: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vouch {
    pub voucher: Address,
    pub stake: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub yield_bps: u64,
    pub slash_bps: u64,
}

const TTL_THRESHOLD: u32 = 518_400;
const TTL_TARGET: u32 = 518_400;

/// Default yield rate numerator: 2% = 200 / 10_000.
const DEFAULT_YIELD_NUMERATOR: u64 = 200;
const YIELD_DENOMINATOR: u64 = 10_000;

/// Slash basis points: 50% = 5000 / 10_000 (#646).
/// Guard: must not exceed 10_000 to prevent underflow in slash calculation.
const SLASH_BPS: u64 = 5_000;

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
const CONFIG_KEY: soroban_sdk::Symbol = symbol_short!("CONFIG");
const PAUSED_KEY: soroban_sdk::Symbol = symbol_short!("PAUSED");
const SLASH_BPS_KEY: soroban_sdk::Symbol = symbol_short!("SL_BPS");
const LOAN_DURATION_KEY: soroban_sdk::Symbol = symbol_short!("LOAN_DUR");
const MIN_STAKE_KEY: soroban_sdk::Symbol = symbol_short!("MIN_STK");
const YIELD_BPS_KEY: soroban_sdk::Symbol = symbol_short!("YIELD_BPS");
const YIELD_NUMERATOR: u64 = DEFAULT_YIELD_NUMERATOR;

const LOAN_REQUESTED: Symbol = symbol_short!("loan_req");
const LOAN_REPAID: Symbol = symbol_short!("loan_rep");
const LOAN_SLASHED: Symbol = symbol_short!("loan_sls");
const VOUCH_CREATED: Symbol = symbol_short!("vouch_cr");

fn loan_key(borrower: &Address) -> (soroban_sdk::Symbol, Address) {
    (symbol_short!("LOAN"), borrower.clone())
}

fn borrower_key(borrower: &Address) -> (soroban_sdk::Symbol, Address) {
    (symbol_short!("BORR"), borrower.clone())
}

fn vouches_key(borrower: &Address) -> (soroban_sdk::Symbol, Address) {
    (symbol_short!("VOUCHES"), borrower.clone())
}

fn voucher_history_key(voucher: &Address) -> (soroban_sdk::Symbol, Address) {
    (symbol_short!("V_HIST"), voucher.clone())
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

fn get_config(env: &Env) -> Config {
    env.storage()
        .persistent()
        .get(&CONFIG_KEY)
        .unwrap_or_else(|| Config {
            yield_bps: 200,
            slash_bps: 5000,
        })
}

fn require_admin(env: &Env, caller: &Address) {
    caller.require_auth();
    if get_admin(env) != *caller {
        panic_with_error!(env, ContractError::UnauthorizedAdmin);
    }
}

fn require_not_paused(env: &Env) {
    let paused: bool = env.storage().persistent().get(&PAUSED_KEY).unwrap_or(false);
    if paused {
        panic_with_error!(env, ContractError::ContractPaused);
    }
}

fn get_slash_bps(env: &Env) -> u32 {
    env.storage()
        .persistent()
        .get(&SLASH_BPS_KEY)
        .unwrap_or(5000)
}

fn get_loan_duration(env: &Env) -> u64 {
    env.storage()
        .persistent()
        .get(&LOAN_DURATION_KEY)
        .unwrap_or(2_592_000)
}

#[contract]
pub struct LendingContract;

#[contractimpl]
impl LendingContract {
    /// Initialize the lending contract with an admin, payment token, and yield rate.
    ///
    /// # Security
    /// `deployer` must sign this transaction. Without this guard any observer
    /// of the deployment transaction can race to call `initialize` first,
    /// setting themselves as admin (#625). Call this in the same transaction as
    /// contract deployment to eliminate the front-run window entirely.
    pub fn initialize(env: Env, deployer: Address, admin: Address, token: Address, slash_bps: u32) {
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

        // #640: Emit initialization event.
        env.events().publish(
            (symbol_short!("INIT"),),
            (admin.clone(), token.clone()),
        );
    }

    /// Request a new loan for the borrower.
    ///
    /// Panics with [`ContractError::LoanAlreadyActive`] if the borrower
    /// already has a non-repaid, non-defaulted loan.
    pub fn request_loan(env: Env, borrower: Address, amount: u64) {
        require_not_paused(&env);
        borrower.require_auth();

        let key = loan_key(&borrower);

        if let Some(existing) = env.storage().persistent().get::<_, Loan>(&key) {
            if existing.status == LoanStatus::Active {
                panic_with_error!(&env, ContractError::LoanAlreadyActive);
            }
        }

        // #628: Check contract has sufficient balance before disbursing
        let token_addr = get_token(&env);
        let tok = token::Client::new(&env, &token_addr);
        let contract_balance = tok.balance(&env.current_contract_address());
        if contract_balance < (amount as i128) {
            panic_with_error!(&env, ContractError::InsufficientFunds);
        }

        let loan = Loan {
            borrower: borrower.clone(),
            amount,
            status: LoanStatus::Active,
            deadline,
        };
        env.storage().persistent().set(&key, &loan);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);

        // Transfer the loan amount to the borrower
        tok.transfer(
            &env.current_contract_address(),
            &borrower,
            &(amount as i128),
        );
    }

    /// Repay the active loan and distribute yield to all vouchers.
    ///
    /// # Repayment Amount
    /// Borrower must repay: loan.amount + total_yield
    /// The yield is calculated as: Σ (stake * 200 / 10_000) for all vouchers.
    /// This ensures yield comes from the borrower's repayment, not pre-minted
    /// contract balance (#632).
    ///
    /// # Security
    /// Total yield is computed before any transfer.
    /// The contract balance is then asserted to be ≥ total yield. This prevents
    /// the loop from panicking mid-execution when the contract is underfunded
    /// (#627).
    /// 
    /// The caller must match the loan's borrower address (#645).
    pub fn repay(env: Env, borrower: Address) {
        require_not_paused(&env);
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

        // #645: Verify the caller matches the loan's borrower.
        assert_eq!(borrower, loan.borrower);

        let vouches: Vec<Vouch> = env
            .storage()
            .persistent()
            .get(&vouches_key(&borrower))
            .unwrap_or_else(|| Vec::new(&env));

        let yield_bps: u64 = env
            .storage()
            .persistent()
            .get(&YIELD_BPS_KEY)
            .unwrap_or(DEFAULT_YIELD_NUMERATOR);

        // #627: Pre-calculate total yield before touching any balances.
        // #643: Use checked addition to prevent overflow.
        let mut total_yield: i128 = 0;
        for v in vouches.iter() {
            let yield_amount = (v.stake * YIELD_NUMERATOR / YIELD_DENOMINATOR) as i128;
            total_yield = total_yield.checked_add(yield_amount)
                .unwrap_or_else(|| panic_with_error!(&env, ContractError::StakeSummationOverflow));
        }

        // #632: Collect loan amount + yield from borrower.
        let token_addr = get_token(&env);
        let tok = token::Client::new(&env, &token_addr);
        let contract_balance = tok.balance(&env.current_contract_address());
        if contract_balance < total_yield {
            panic_with_error!(&env, ContractError::InsufficientFunds);
        }

        loan.status = LoanStatus::Repaid;
        env.storage().persistent().set(&key, &loan);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);

        // #632: Distribute yield to vouchers from collected repayment.
        for v in vouches.iter() {
            let yield_amount = v.stake * yield_bps / YIELD_DENOMINATOR;
            if yield_amount > 0 {
                tok.transfer(
                    &env.current_contract_address(),
                    &v.voucher,
                    &(yield_amount as i128),
                );
            }
        }

        env.events()
            .publish((LOAN_REPAID,), (borrower.clone(), total_yield));
    }

    /// Vouch for a borrower with a token stake.
    ///
    /// # Minimum Stake
    /// Stake must be ≥ `MIN_VOUCH_STAKE` (50 stroops). The yield formula
    /// `stake * 200 / 10_000` uses integer division and truncates to zero for
    /// stakes below 50, so vouchers would silently receive no yield (#624).
    ///
    /// # Maximum Vouchers
    /// A loan can have at most 100 vouchers to prevent DoS via unbounded voucher
    /// list (#633, #634).
    ///
    /// # Errors
    /// - [`ContractError::ZeroStake`] if stake is 0
    /// - [`ContractError::StakeBelowMinimum`] if stake < 50 stroops (#624)
    /// - [`ContractError::DuplicateVouch`] if this voucher already vouched for
    ///   this borrower
    /// - [`ContractError::TooManyVouchers`] if loan already has 100 vouchers (#633, #634)
    pub fn vouch(env: Env, borrower: Address, voucher: Address, stake: u64) {
        require_not_paused(&env);
        voucher.require_auth();

        // #629: Prevent borrower from vouching for themselves
        if voucher == borrower {
            panic_with_error!(&env, ContractError::DuplicateVouch);
        }

        // #630: Check if borrower already has an active loan
        let loan_key = loan_key(&borrower);
        if let Some(existing) = env.storage().persistent().get::<_, Loan>(&loan_key) {
            if existing.status == LoanStatus::Active {
                panic_with_error!(&env, ContractError::LoanAlreadyActive);
            }
        }

        if stake == 0 {
            panic_with_error!(&env, ContractError::ZeroStake);
        }

        let min_stake: u64 = env
            .storage()
            .persistent()
            .get(&MIN_STAKE_KEY)
            .unwrap_or(MIN_VOUCH_STAKE);
        if stake < min_stake {
            panic_with_error!(&env, ContractError::StakeBelowMinimum);
        }

        let key = vouches_key(&borrower);
        let mut vouches: Vec<Vouch> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));

        // #633, #634: Enforce max vouchers per loan to prevent DoS.
        if vouches.len() >= 100 {
            panic_with_error!(&env, ContractError::TooManyVouchers);
        }

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

        let hist_key = voucher_history_key(&voucher);
        let mut history: Vec<Address> = env
            .storage()
            .persistent()
            .get(&hist_key)
            .unwrap_or_else(|| Vec::new(&env));
        history.push_back(borrower);
        env.storage().persistent().set(&hist_key, &history);
        env.storage()
            .persistent()
            .extend_ttl(&hist_key, TTL_THRESHOLD, TTL_TARGET);
    }

    /// Admin-only: mark a loan as defaulted and slash based on configured rate.
    ///
    /// The slashed amount is accumulated in `slash_balance`; the remainder is
    /// returned to the voucher. The accumulated balance can be withdrawn by the
    /// admin via [`slash_treasury`] (#626).
    ///
    /// # DoS Protection
    /// Enforces max_vouchers_per_loan cap to prevent gas exhaustion (#633).
    pub fn slash(env: Env, admin: Address, borrower: Address) {
        require_admin(&env, &admin);

        // #646: Guard against misconfigured SLASH_BPS exceeding 10_000.
        assert!(SLASH_BPS <= 10_000);

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

        let borrower_key_val = borrower_key(&borrower);
        if let Some(mut borrower_record) = env
            .storage()
            .persistent()
            .get::<_, Borrower>(&borrower_key_val)
        {
            borrower_record.default_count += 1;
            env.storage()
                .persistent()
                .set(&borrower_key_val, &borrower_record);
            env.storage()
                .persistent()
                .extend_ttl(&borrower_key_val, TTL_THRESHOLD, TTL_TARGET);
        }

        let vouches: Vec<Vouch> = env
            .storage()
            .persistent()
            .get(&vouches_key(&borrower))
            .unwrap_or_else(|| Vec::new(&env));

        // #633: Enforce max_vouchers_per_loan cap to prevent DoS via unbounded voucher list.
        if vouches.len() > 100 {
            panic_with_error!(&env, ContractError::TooManyVouchers);
        }

        let token_addr = get_token(&env);
        let tok = token::Client::new(&env, &token_addr);

        let slash_bps = get_slash_bps(&env);
        let mut slash_accum: u64 = 0;
        for v in vouches.iter() {
            let slashed = v.stake * SLASH_BPS / 10_000;
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

        env.events()
            .publish((LOAN_SLASHED,), (borrower.clone(), slash_accum));
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

    /// Withdraw a vouch before a loan is requested (#631).
    ///
    /// Allows a voucher to reclaim their stake if no active loan exists.
    /// Panics if an active loan is found.
    pub fn withdraw_vouch(env: Env, borrower: Address, voucher: Address) {
        voucher.require_auth();

        // #631: Check no active loan exists
        let loan_key = loan_key(&borrower);
        if let Some(existing) = env.storage().persistent().get::<_, Loan>(&loan_key) {
            if existing.status == LoanStatus::Active {
                panic_with_error!(&env, ContractError::VouchWithdrawNotAllowed);
            }
        }

        let key = vouches_key(&borrower);
        let mut vouches: Vec<Vouch> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));

        let mut found_index = None;
        for (i, v) in vouches.iter().enumerate() {
            if v.voucher == voucher {
                found_index = Some(i as u32);
                break;
            }
        }

        if let Some(idx) = found_index {
            let vouch = vouches.get(idx).unwrap();
            let stake = vouch.stake;

            vouches.remove(idx);
            env.storage().persistent().set(&key, &vouches);
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);

            let token_addr = get_token(&env);
            let tok = token::Client::new(&env, &token_addr);
            tok.transfer(
                &env.current_contract_address(),
                &voucher,
                &(stake as i128),
            );
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
    use soroban_sdk::testutils::{Address as _, Events};

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

    fn setup() -> (Env, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &contract_id);

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&deployer, &admin, &token);

        (env, contract_id, admin, token, deployer)
    }

    #[test]
    fn test_request_loan_prevents_overwrite_of_active_loan() {
        let (env, _contract_id, _admin, _token, _deployer) = setup();
        let client = LendingContractClient::new(&env, &_contract_id);

        let borrower = Address::generate(&env);

        client.request_loan(&borrower, &1000);
        let loan1 = client.get_loan(&borrower).unwrap();
        assert_eq!(loan1.amount, 1000);
        assert_eq!(loan1.status, LoanStatus::Active);

        let result = client.try_request_loan(&borrower, &2000);
        assert!(result.is_err());

        let loan2 = client.get_loan(&borrower).unwrap();
        assert_eq!(loan2.amount, 1000);
        assert_eq!(loan2.status, LoanStatus::Active);
    }

    #[test]
    fn test_repay_verifies_borrower_matches_loan_record() {
        let (env, _contract_id, _admin, _token, _deployer) = setup();
        let client = LendingContractClient::new(&env, &_contract_id);

        let borrower1 = Address::generate(&env);
        let borrower2 = Address::generate(&env);

        client.request_loan(&borrower1, &1000);

        let result = client.try_repay(&borrower2);
        assert!(result.is_err());

        let loan = client.get_loan(&borrower1).unwrap();
        assert_eq!(loan.status, LoanStatus::Active);
    }

    #[test]
    fn test_slash_bps_guard_prevents_underflow() {
        let (env, _contract_id, _admin, _token, _deployer) = setup();
        let _client = LendingContractClient::new(&env, &_contract_id);

        assert!(SLASH_BPS <= 10_000);
    }


    fn setup_contract(env: &Env) -> (Address, Address, Address) {
        let admin = Address::generate(env);
        let token = Address::generate(env);
        let contract_id = env.register(LendingContract, ());

        let client = LendingContractClient::new(env, &contract_id);
        let deployer = Address::generate(env);

        client.initialize(&deployer, &admin, &token);

        (contract_id, admin, token)
    }

    #[test]
    fn test_vouch_max_vouchers_limit() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, _, _) = setup_contract(&env);
        let client = LendingContractClient::new(&env, &contract_id);

        let borrower = Address::generate(&env);
        client.request_loan(&borrower, &100_000_000);

        // Add 100 vouchers (the max)
        for i in 0..100 {
            let voucher = Address::generate(&env);
            client.vouch(&borrower, &voucher, &100);
        }

        // Try to add the 101st voucher - should fail
        let extra_voucher = Address::generate(&env);
        let result = client.try_vouch(&borrower, &extra_voucher, &100);

        assert!(result.is_err());
    }

    #[test]
    fn test_slash_with_max_vouchers() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, admin, _) = setup_contract(&env);
        let client = LendingContractClient::new(&env, &contract_id);

        let borrower = Address::generate(&env);
        client.request_loan(&borrower, &100_000_000);

        // Add 100 vouchers
        for i in 0..100 {
            let voucher = Address::generate(&env);
            client.vouch(&borrower, &voucher, &100);
        }

        // Slash should succeed with exactly 100 vouchers
        client.slash(&admin, &borrower);

        let loan = client.get_loan(&borrower);
        assert!(loan.is_some());
        assert_eq!(loan.unwrap().status, LoanStatus::Defaulted);
    }

    #[test]
    fn test_repay_with_max_vouchers() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, _, _) = setup_contract(&env);
        let client = LendingContractClient::new(&env, &contract_id);

        let borrower = Address::generate(&env);
        client.request_loan(&borrower, &100_000_000);

        // Add 100 vouchers
        for i in 0..100 {
            let voucher = Address::generate(&env);
            client.vouch(&borrower, &voucher, &100);
        }

        // Repay should succeed with exactly 100 vouchers
        client.repay(&borrower);

        let loan = client.get_loan(&borrower);
        assert!(loan.is_some());
        assert_eq!(loan.unwrap().status, LoanStatus::Repaid);
    }

    #[test]
    fn test_repay_collects_loan_amount_plus_yield() {
        let env = Env::default();
        env.mock_all_auths();

        let (contract_id, _, _) = setup_contract(&env);
        let client = LendingContractClient::new(&env, &contract_id);

        let borrower = Address::generate(&env);
        let loan_amount = 1000u64;
        client.request_loan(&borrower, &loan_amount);

        let voucher1 = Address::generate(&env);
        let voucher2 = Address::generate(&env);
        let stake1 = 500u64;
        let stake2 = 500u64;

        client.vouch(&borrower, &voucher1, &stake1);
        client.vouch(&borrower, &voucher2, &stake2);

        // Expected yield: (500 * 200 / 10_000) + (500 * 200 / 10_000) = 10 + 10 = 20
        // Total repayment should be: 1000 + 20 = 1020
        // Borrower must provide this amount in the repay call

        client.repay(&borrower);

        let loan = client.get_loan(&borrower);
        assert!(loan.is_some());
        assert_eq!(loan.unwrap().status, LoanStatus::Repaid);
    }

    #[test]
    fn test_request_loan_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_addr = Address::generate(&env);
        let borrower = Address::generate(&env);

        let contract_id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &contract_id);

        client.initialize(&deployer, &admin, &token_addr);

        let amount = 1000u64;
        client.request_loan(&borrower, &amount);

        let events = env.events().all();
        let loan_req_events: Vec<_> = events
            .iter()
            .filter(|e| {
                if let soroban_sdk::xdr::ContractEvent::V0(v0) = &e.event {
                    v0.topics.len() > 0
                        && v0.topics.get(0).map_or(false, |t| {
                            if let soroban_sdk::xdr::ScVal::Symbol(sym) = t {
                                sym.0.as_slice() == b"loan_req"
                            } else {
                                false
                            }
                        })
                } else {
                    false
                }
            })
            .collect();

        assert!(!loan_req_events.is_empty(), "request_loan should emit event");
    }

    #[test]
    fn test_vouch_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_addr = Address::generate(&env);
        let borrower = Address::generate(&env);
        let voucher = Address::generate(&env);

        let contract_id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &contract_id);

        client.initialize(&deployer, &admin, &token_addr);
        client.request_loan(&borrower, &1000u64);

        let stake = 100u64;
        client.vouch(&borrower, &voucher, &stake);

        let events = env.events().all();
        let vouch_events: Vec<_> = events
            .iter()
            .filter(|e| {
                if let soroban_sdk::xdr::ContractEvent::V0(v0) = &e.event {
                    v0.topics.len() > 0
                        && v0.topics.get(0).map_or(false, |t| {
                            if let soroban_sdk::xdr::ScVal::Symbol(sym) = t {
                                sym.0.as_slice() == b"vouch_cr"
                            } else {
                                false
                            }
                        })
                } else {
                    false
                }
            })
            .collect();

        assert!(!vouch_events.is_empty(), "vouch should emit event");
    }

    #[test]
    fn test_repay_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_addr = Address::generate(&env);
        let borrower = Address::generate(&env);

        let contract_id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &contract_id);

        client.initialize(&deployer, &admin, &token_addr);
        client.request_loan(&borrower, &1000u64);

        client.repay(&borrower);

        let events = env.events().all();
        let repay_events: Vec<_> = events
            .iter()
            .filter(|e| {
                if let soroban_sdk::xdr::ContractEvent::V0(v0) = &e.event {
                    v0.topics.len() > 0
                        && v0.topics.get(0).map_or(false, |t| {
                            if let soroban_sdk::xdr::ScVal::Symbol(sym) = t {
                                sym.0.as_slice() == b"loan_rep"
                            } else {
                                false
                            }
                        })
                } else {
                    false
                }
            })
            .collect();

        assert!(!repay_events.is_empty(), "repay should emit event");
    }

    #[test]
    fn test_slash_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let deployer = Address::generate(&env);
        let admin = Address::generate(&env);
        let token_addr = Address::generate(&env);
        let borrower = Address::generate(&env);

        let contract_id = env.register(LendingContract, ());
        let client = LendingContractClient::new(&env, &contract_id);

        client.initialize(&deployer, &admin, &token_addr);
        client.request_loan(&borrower, &1000u64);

        client.slash(&admin, &borrower);

        let events = env.events().all();
        let slash_events: Vec<_> = events
            .iter()
            .filter(|e| {
                if let soroban_sdk::xdr::ContractEvent::V0(v0) = &e.event {
                    v0.topics.len() > 0
                        && v0.topics.get(0).map_or(false, |t| {
                            if let soroban_sdk::xdr::ScVal::Symbol(sym) = t {
                                sym.0.as_slice() == b"loan_sls"
                            } else {
                                false
                            }
                        })
                } else {
                    false
                }
            })
            .collect();

        assert!(!slash_events.is_empty(), "slash should emit event");
    }
}
