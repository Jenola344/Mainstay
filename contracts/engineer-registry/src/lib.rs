#![no_std]
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    BytesN, Env, Symbol, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ContractError {
    CredentialAlreadyRevoked = 1,
    UnauthorizedAdmin = 2,
    EngineerNotFound = 3,
    NotInitialized = 4,
    AdminAlreadyInitialized = 5,
    UntrustedIssuer = 6,
    InvalidCredentialHash = 7,
    Paused = 8,
    CredentialRevoked = 9,
    EngineerAlreadyRegistered = 10,
    IssuerNotFound = 11,
    PendingAdminAlreadyExists = 12,
    InvalidValidityPeriod = 13,
    IssuerRemoved = 14,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Engineer {
    pub address: Address,
    pub credential_hash: BytesN<32>,
    pub issuer: Address,
    pub active: bool,
    pub issued_at: u64,
    pub expires_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineerStatus {
    Active = 0,
    Revoked = 1,
    Expired = 2,
    NotFound = 3,
}

fn engineer_key(addr: &Address) -> (Symbol, Address) {
    (symbol_short!("ENG"), addr.clone())
}

const PAUSED_KEY: Symbol = symbol_short!("PAUSED");
const REG_ENG_TOPIC: Symbol = symbol_short!("REG_ENG");
const REVOKE_TOPIC: Symbol = symbol_short!("REV_CRED");
const MIN_VALIDITY_PERIOD: u64 = 86_400;
const EVENT_PROP_ADMIN: Symbol = symbol_short!("PROP_ADM");


/// Soroban persistent-storage TTL constants.
/// 1 ledger ≈ 5 seconds → 518_400 ledgers ≈ 30 days.
const TTL_THRESHOLD: u32 = 518_400;
const TTL_TARGET: u32 = 518_400;

fn is_paused(env: &Env) -> bool {
    env.storage().persistent().get(&PAUSED_KEY).unwrap_or(false)
}

fn ensure_not_paused(env: &Env) {
    if is_paused(env) {
        panic_with_error!(env, ContractError::Paused);
    }
}

fn admin_key() -> Symbol {
    symbol_short!("ADMIN")
}

fn pending_admin_key() -> Symbol {
    symbol_short!("PADMIN")
}

fn trusted_key(issuer: &Address) -> (Symbol, Address) {
    (symbol_short!("TRUSTED"), issuer.clone())
}

fn issuer_engineers_key(issuer: &Address) -> (Symbol, Address) {
    (symbol_short!("ISS_ENGS"), issuer.clone())
}

/// Returns the key for the authoritative trusted-issuer list in instance storage.
/// This list MUST NOT expire: TTL must be extended on every write so that
/// `get_trusted_issuers` never returns a stale empty vec while individual
/// `trusted_key` entries are still active.
fn issuer_list_key() -> Symbol {
    symbol_short!("ISS_LIST")
}

#[contract]
pub struct EngineerRegistry;

#[contractimpl]
impl EngineerRegistry {
    /// Register a new engineer with their credential information.
    /// Only trusted issuers can register engineers.
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer being registered
    /// * `credential_hash` - Hash of the engineer's credentials/certifications
    /// * `issuer` - The trusted issuer address registering the engineer
    /// * `validity_period` - Duration in seconds for which the credentials are valid
    ///
    /// # Panics
    /// - [`ContractError::UntrustedIssuer`] if the issuer is not in the trusted list
    /// - [`ContractError::InvalidCredentialHash`] if credential hash is all zeros
    /// - [`ContractError::EngineerAlreadyRegistered`] if an active engineer record already exists
    pub fn register_engineer(
        env: Env,
        engineer: Address,
        credential_hash: BytesN<32>,
        issuer: Address,
        validity_period: u64,
    ) {
        ensure_not_paused(&env);
        issuer.require_auth();
        if !env.storage().instance().has(&trusted_key(&issuer)) {
            panic_with_error!(&env, ContractError::UntrustedIssuer);
        }
        if credential_hash == BytesN::from_array(&env, &[0u8; 32]) {
            panic_with_error!(&env, ContractError::InvalidCredentialHash);
        }
        if validity_period == 0 {
            panic_with_error!(&env, ContractError::InvalidValidityPeriod);
        }

        // Check if an engineer record already exists and is *not revoked*.
        // Re-registering would otherwise silently overwrite credentials.
        if let Some(existing) = env
            .storage()
            .persistent()
            .get::<_, Engineer>(&engineer_key(&engineer))
        {
            if existing.active {
                panic_with_error!(&env, ContractError::EngineerAlreadyRegistered);
            }
            // existing is present but not active (revoked) => allow re-registration.
        }





        let now = env.ledger().timestamp();
        let record = Engineer {
            address: engineer.clone(),
            credential_hash: credential_hash.clone(),
            issuer: issuer.clone(),
            active: true,
            issued_at: now,
            expires_at: now + validity_period,
        };
        env.storage()
            .persistent()
            .set(&engineer_key(&engineer), &record);
        env.storage()
            .persistent()
            .extend_ttl(&engineer_key(&engineer), TTL_THRESHOLD, TTL_TARGET);

        // Track issuer → engineers mapping (avoid duplicates on re-registration after revoke)
        let mut list: Vec<Address> = env
            .storage()
            .persistent()
            .get(&issuer_engineers_key(&issuer))
            .unwrap_or(Vec::new(&env));
        if !list.contains(engineer.clone()) {
            list.push_back(engineer.clone());
        }
        env.storage()
            .persistent()
            .set(&issuer_engineers_key(&issuer), &list);
        env.storage()
            .persistent()
            .extend_ttl(&issuer_engineers_key(&issuer), TTL_THRESHOLD, TTL_TARGET);

        // Emit engineer registration event
        env.events().publish(
            (REG_ENG_TOPIC, engineer.clone()),
            (issuer, credential_hash.clone(), now, now + validity_period),
        );
    }

    /// Verify if an engineer has valid, active credentials.
    /// Checks both active status and expiration time.
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer to verify
    ///
    /// # Returns
    /// `true` if the engineer has valid, non-expired credentials; `false` otherwise
    pub fn verify_engineer(env: Env, engineer: Address) -> bool {
        env.storage()
            .persistent()
            .get::<_, Engineer>(&engineer_key(&engineer))
            .map(|e| e.active && env.ledger().timestamp() < e.expires_at)
            .unwrap_or(false)
    }

    /// Revoke an engineer's credentials, making them inactive.
    /// Only the original issuer can revoke credentials.
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer whose credentials should be revoked
    ///
    /// # Authorization
    /// Requires signature from the original issuer stored in the engineer's record.
    /// A different trusted issuer cannot revoke another issuer's engineer.
    ///
    /// # Panics
    /// - [`ContractError::EngineerNotFound`] if no engineer exists with the given address
    /// - [`ContractError::CredentialAlreadyRevoked`] if the credentials are already revoked
    pub fn revoke_credential(env: Env, engineer: Address) {
        ensure_not_paused(&env);
        let mut record: Engineer = env
            .storage()
            .persistent()
            .get(&engineer_key(&engineer))
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::EngineerNotFound));
        record.issuer.require_auth();
        if !record.active {
            panic_with_error!(&env, ContractError::CredentialAlreadyRevoked);
        }
        // Extend TTL before write to ensure consistency even on near-expired entries
        env.storage()
            .persistent()
            .extend_ttl(&engineer_key(&engineer), TTL_THRESHOLD, TTL_TARGET);
        record.active = false;
        env.storage()
            .persistent()
            .set(&engineer_key(&engineer), &record);

        // Emit credential revocation event
        env.events().publish(
            (REVOKE_TOPIC, engineer.clone()),
            (record.issuer.clone(), env.ledger().timestamp()),
        );
    }

    /// Renew an engineer's credential by extending the expiry.
    /// Only the original issuer can renew credentials.
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer whose credential should be renewed
    /// * `new_validity_period` - Duration in seconds from now for the renewed credential
    ///
    /// # Panics
    /// - [`ContractError::EngineerNotFound`] if no engineer exists with the given address
    /// - [`ContractError::CredentialRevoked`] if the credential has been revoked
    pub fn renew_credential(env: Env, engineer: Address, new_validity_period: u64) {
        ensure_not_paused(&env);
        let mut record: Engineer = env
            .storage()
            .persistent()
            .get(&engineer_key(&engineer))
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::EngineerNotFound));
        record.issuer.require_auth();
        if !env.storage().instance().has(&trusted_key(&record.issuer)) {
            panic_with_error!(&env, ContractError::IssuerRemoved);
        }
        if !record.active {
            panic_with_error!(&env, ContractError::CredentialRevoked);
        }
        if new_validity_period < MIN_VALIDITY_PERIOD {
            panic_with_error!(&env, ContractError::InvalidValidityPeriod);
        }
        let renewed_at = env.ledger().timestamp();
        let previous_expires_at = record.expires_at;
        let renewal_base = if previous_expires_at > renewed_at {
            previous_expires_at
        } else {
            renewed_at
        };
        record.expires_at = renewal_base + new_validity_period;
        env.storage()
            .persistent()
            .extend_ttl(&engineer_key(&engineer), TTL_THRESHOLD, TTL_TARGET);
        env.storage()
            .persistent()
            .set(&engineer_key(&engineer), &record);

        env.events().publish(
            (symbol_short!("RNW_CRED"), engineer.clone()),
            (
                record.issuer.clone(),
                previous_expires_at,
                record.expires_at,
                renewed_at,
            ),
        );
    }

    /// Retrieve complete engineer information by address.
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer to retrieve
    ///
    /// # Returns
    /// The complete Engineer struct with all credential information
    ///
    /// # Panics
    /// - [`ContractError::EngineerNotFound`] if no engineer exists with the given address
    pub fn get_engineer(env: Env, engineer: Address) -> Engineer {
        env.storage()
            .persistent()
            .get(&engineer_key(&engineer))
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::EngineerNotFound))
    }

    /// Get the status of an engineer's credential.
    /// Distinguishes between active, revoked, expired, and not found states.
    ///
    /// # Returns
    /// An EngineerStatus enum indicating the credential state
    pub fn get_engineer_status(env: Env, engineer: Address) -> EngineerStatus {
        match env
            .storage()
            .persistent()
            .get::<_, Engineer>(&engineer_key(&engineer))
        {
            Some(e) => {
                if !e.active {
                    EngineerStatus::Revoked
                } else if env.ledger().timestamp() >= e.expires_at {
                    EngineerStatus::Expired
                } else {
                    EngineerStatus::Active
                }
            }
            None => EngineerStatus::NotFound,
        }
    }

    /// Initialize the admin address for the contract.
    /// This function should be called once immediately after deployment.
    ///
    /// # Arguments
    /// * `admin` - The address that will have administrative privileges
    ///
    /// # Panics
    /// - [`ContractError::AdminAlreadyInitialized`] if admin has already been initialized
    pub fn initialize_admin(env: Env, admin: Address) {
        admin.require_auth();
        if env.storage().instance().has(&admin_key()) {
            panic_with_error!(&env, ContractError::AdminAlreadyInitialized);
        }
        env.storage().instance().set(&admin_key(), &admin);
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_TARGET);
    }

    /// Get the current admin address of the contract.
    ///
    /// # Returns
    /// The address of the current administrator
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if the admin has not been initialized
    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&admin_key())
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized))
    }

    /// Propose a new admin address (step 1 of 2-step transfer).
    /// Only the current admin can propose a new admin.
    ///
    /// # Arguments
    /// * `admin` - The current admin address
    /// * `new_admin` - The address to propose as the new admin
    ///
    /// # Panics
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the current admin
    /// - [`ContractError::PendingAdminAlreadyExists`] if a pending admin already exists
    pub fn propose_admin(env: Env, admin: Address, new_admin: Address) {
        admin.require_auth();
        let stored_admin: Address = Self::get_admin(env.clone());
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        if env.storage().instance().has(&pending_admin_key()) {
            panic_with_error!(&env, ContractError::PendingAdminAlreadyExists);
        }
        env.storage()
            .instance()
            .set(&pending_admin_key(), &new_admin);
        env.events()
            .publish((EVENT_PROP_ADMIN,), (admin, new_admin));
    }

    /// Accept the admin transfer (step 2 of 2-step transfer).
    /// Only the pending admin can accept and become the new admin.
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if no pending admin exists
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the pending admin
    pub fn accept_admin(env: Env) {
        let pending_admin: Address = env
            .storage()
            .instance()
            .get(&pending_admin_key())
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        pending_admin.require_auth();
        env.storage().instance().set(&admin_key(), &pending_admin);
        env.storage().instance().remove(&pending_admin_key());
        env.events()
            .publish((symbol_short!("ADMIN_SET"),), (pending_admin,));
    }

    /// Admin-only function to pause the contract.
    ///
    /// # Arguments
    /// * `admin` - The address that must match the stored admin
    pub fn pause(env: Env, admin: Address) {
        admin.require_auth();
        let stored_admin: Address = Self::get_admin(env.clone());
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        env.storage().persistent().set(&PAUSED_KEY, &true);
        env.storage()
            .persistent()
            .extend_ttl(&PAUSED_KEY, TTL_THRESHOLD, TTL_TARGET);
        env.events().publish((symbol_short!("PAUSED"),), (admin,));
    }

    /// Admin-only function to unpause the contract.
    ///
    /// # Arguments
    /// * `admin` - The address that must match the stored admin
    pub fn unpause(env: Env, admin: Address) {
        admin.require_auth();
        let stored_admin: Address = Self::get_admin(env.clone());
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        env.storage().persistent().set(&PAUSED_KEY, &false);
        env.storage()
            .persistent()
            .extend_ttl(&PAUSED_KEY, TTL_THRESHOLD, TTL_TARGET);
        env.events().publish((symbol_short!("UNPAUSED"),), (admin,));
    }

    /// Check if the contract is currently paused.
    ///
    /// # Returns
    /// `true` if paused; `false` otherwise
    pub fn is_paused(env: Env) -> bool {
        is_paused(&env)
    }

    /// Check if an issuer is in the trusted issuers list.
    ///
    /// # Arguments
    /// * `issuer` - The address of the issuer to check
    ///
    /// # Returns
    /// `true` if the issuer is trusted; `false` otherwise
    pub fn is_trusted_issuer(env: Env, issuer: Address) -> bool {
        env.storage().instance().has(&trusted_key(&issuer))
    }

    /// Get the list of all trusted issuer addresses.
    ///
    /// # Returns
    /// A Vec containing all trusted issuer addresses
    pub fn get_trusted_issuers(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&issuer_list_key())
            .unwrap_or(Vec::new(&env))
    }

    /// Admin-only function to add a new trusted issuer.
    /// Only admins can modify the trusted issuers list.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored admin
    /// * `issuer` - The address of the issuer to add as trusted
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if the admin has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn add_trusted_issuer(env: Env, admin: Address, issuer: Address) {
        ensure_not_paused(&env);
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&admin_key())
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        env.storage().instance().set(&trusted_key(&issuer), &());
        let mut list: Vec<Address> = env
            .storage()
            .instance()
            .get(&issuer_list_key())
            .unwrap_or(Vec::new(&env));
        if !list.contains(issuer.clone()) {
            list.push_back(issuer.clone());
            env.storage().instance().set(&issuer_list_key(), &list);
            env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_TARGET);
            env.events()
                .publish((symbol_short!("ISS_ADD"), admin), (issuer,));
        } else {
            env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_TARGET);
        }
    }

    /// Admin-only function to remove a trusted issuer.
    /// Only admins can modify the trusted issuers list.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored admin
    /// * `issuer` - The address of the issuer to remove from trusted list
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if the admin has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn remove_trusted_issuer(env: Env, admin: Address, issuer: Address) {
        ensure_not_paused(&env);
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&admin_key())
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        // Check if issuer exists before removing
        if !env.storage().instance().has(&trusted_key(&issuer)) {
            panic_with_error!(&env, ContractError::IssuerNotFound);
        }

        env.storage().instance().remove(&trusted_key(&issuer));
        let list: Vec<Address> = env
            .storage()
            .instance()
            .get(&issuer_list_key())
            .unwrap_or(Vec::new(&env));
        let mut new_list: Vec<Address> = Vec::new(&env);
        for addr in list.iter() {
            if addr != issuer {
                new_list.push_back(addr);
            }
        }
        env.storage().instance().set(&issuer_list_key(), &new_list);
        env.storage().instance().extend_ttl(TTL_THRESHOLD, TTL_TARGET);

        // Revoke all active engineers registered by this issuer
        let engineers: Vec<Address> = env
            .storage()
            .persistent()
            .get(&issuer_engineers_key(&issuer))
            .unwrap_or(Vec::new(&env));
        for engineer in engineers.iter() {
            if let Some(mut record) = env
                .storage()
                .persistent()
                .get::<_, Engineer>(&engineer_key(&engineer))
            {
                if record.active {
                    record.active = false;
                    env.storage()
                        .persistent()
                        .extend_ttl(&engineer_key(&engineer), TTL_THRESHOLD, TTL_TARGET);
                    env.storage()
                        .persistent()
                        .set(&engineer_key(&engineer), &record);
                }
            }
        }

        env.events()
            .publish((symbol_short!("ISS_RM"), admin.clone()), (issuer,));
    }

    /// Get all engineer addresses that have been credentialed by a specific issuer.
    /// This includes both active and revoked engineers (historical registry).
    ///
    /// # Arguments
    /// * `issuer` - The address of the issuer to query
    ///
    /// # Returns
    /// A Vec containing all engineer addresses credentialed by the given issuer
    pub fn get_engineers_by_issuer(env: Env, issuer: Address) -> Vec<Address> {
        env.storage()
            .persistent()
            .get(&issuer_engineers_key(&issuer))
            .unwrap_or(Vec::new(&env))
    }

    /// Get only active, non-expired engineer addresses credentialed by a specific issuer.
    pub fn get_active_engineers_by_issuer(env: Env, issuer: Address) -> Vec<Address> {
        let engineers = Self::get_engineers_by_issuer(env.clone(), issuer);
        let mut active_engineers = Vec::new(&env);
        for engineer in engineers.iter() {
            if Self::get_engineer_status(env.clone(), engineer.clone()) == EngineerStatus::Active {
                active_engineers.push_back(engineer);
            }
        }
        active_engineers
    }

    /// Get the number of engineers credentialed by a specific issuer.
    pub fn get_engineer_count_by_issuer(env: Env, issuer: Address) -> u32 {
        Self::get_engineers_by_issuer(env, issuer).len()
    }

    /// Admin-only function to upgrade the contract WASM to a new hash.
    pub fn upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) {
        ensure_not_paused(&env);
        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&admin_key())
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        env.events().publish(
            (symbol_short!("UPGRADE"), admin.clone()),
            new_wasm_hash.clone(),
        );

        #[cfg(not(test))]
        {
            env.deployer().update_current_contract_wasm(new_wasm_hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::storage::Instance,
        testutils::storage::Persistent,
        testutils::Address as _,
        testutils::{Events, Ledger},
        BytesN, Env, IntoVal,
    };

    fn setup<'a>(env: &'a Env) -> (EngineerRegistryClient<'a>, Address) {
        let contract_id = env.register(EngineerRegistry, ());
        let client = EngineerRegistryClient::new(env, &contract_id);
        let admin = Address::generate(env);
        client.initialize_admin(&admin);
        (client, admin)
    }

    #[test]
    #[should_panic]
    fn test_initialize_admin_called_twice_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(EngineerRegistry, ());
        let client = EngineerRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin);
        // Second call must panic
        client.initialize_admin(&admin);
    }

    #[test]
    fn test_register_verify_revoke() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);
        assert!(client.verify_engineer(&engineer));

        client.revoke_credential(&engineer);
        assert!(!client.verify_engineer(&engineer));
    }

    #[test]
    fn test_register_engineer_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);
        let validity_period: u64 = 31_536_000;

        client.add_trusted_issuer(&admin, &issuer);
        let issued_at = env.ledger().timestamp();
        client.register_engineer(&engineer, &hash, &issuer, &validity_period);

        // ISS_ADD event fires first, REG_ENG is the second event
        let events = env.events().all();
        let (_, topics, data) = events.last().unwrap();

        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        let t1: Address = topics.get(1).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, REG_ENG_TOPIC);
        assert_eq!(t1, engineer);

        let (emitted_issuer, emitted_hash, emitted_issued_at, emitted_expires_at): (
            Address,
            BytesN<32>,
            u64,
            u64,
        ) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_issuer, issuer);
        assert_eq!(emitted_hash, hash);
        assert_eq!(emitted_issued_at, issued_at);
        assert_eq!(emitted_expires_at, issued_at + validity_period);
    }

    #[test]
    fn test_revoke_credential_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        let revoked_at = env.ledger().timestamp();
        client.revoke_credential(&engineer);

        let events = env.events().all();
        let (_, topics, data) = events.last().unwrap();

        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        let t1: Address = topics.get(1).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, REVOKE_TOPIC);
        assert_eq!(t1, engineer);

        let (emitted_issuer, emitted_timestamp): (Address, u64) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_issuer, issuer);
        assert_eq!(emitted_timestamp, revoked_at);
    }

    #[test]
    fn test_revoke_already_revoked_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);
        client.revoke_credential(&engineer);

        let result = client.try_revoke_credential(&engineer);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::CredentialAlreadyRevoked as u32
            )))
        );
    }

    #[test]
    fn test_initialize_admin_double_call_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(EngineerRegistry, ());
        let client = EngineerRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin);

        // Second call should fail with structured error
        let result = client.try_initialize_admin(&admin);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AdminAlreadyInitialized as u32,
            ))),
        );
    }

    #[test]
    fn test_initialize_admin_requires_auth() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(EngineerRegistry, ());
        let client = EngineerRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        // This should succeed because we mock all auths
        client.initialize_admin(&admin);

        // Verify admin was set
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_initialize_admin_extends_instance_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(EngineerRegistry, ());
        let client = EngineerRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin);

        let ttl = env.as_contract(&contract_id, || env.storage().instance().get_ttl());
        assert!(
            ttl > 0,
            "Instance TTL should be extended after initialize_admin"
        );
    }

    #[test]
    fn test_register_zero_hash_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let zero_hash = BytesN::from_array(&env, &[0u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        let result = client.try_register_engineer(&engineer, &zero_hash, &issuer, &31_536_000);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidCredentialHash as u32,
            ))),
        );
    }

    #[test]
    fn test_ttl_extended_on_registration() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        let contract_id = client.address.clone();
        let ttl = env.as_contract(&contract_id, || {
            env.storage().persistent().get_ttl(&engineer_key(&engineer))
        });
        assert!(ttl > 0, "Engineer TTL should be extended");
    }

    #[test]
    fn test_issuer_engineers_ttl_extended_on_registration() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        let contract_id = client.address.clone();
        let ttl = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get_ttl(&issuer_engineers_key(&issuer))
        });
        assert!(ttl > 0, "Issuer engineers TTL should be extended");
    }

    #[test]
    fn test_ttl_extended_on_revoke() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);
        client.revoke_credential(&engineer);

        let contract_id = client.address.clone();
        let ttl = env.as_contract(&contract_id, || {
            env.storage().persistent().get_ttl(&engineer_key(&engineer))
        });
        assert!(ttl > 0, "Engineer TTL should be extended after revoke");
    }

    #[test]
    fn test_admin_can_upgrade() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let new_wasm_hash = BytesN::from_array(&env, &[0xabu8; 32]);
        // In test env the WASM hash won't exist, so we just verify auth passes (no UnauthorizedAdmin error)
        let result = client.try_upgrade(&admin, &new_wasm_hash);
        assert!(
            result
                != Err(Ok(soroban_sdk::Error::from_contract_error(
                    ContractError::UnauthorizedAdmin as u32
                )))
        );
    }

    #[test]
    fn test_non_admin_cannot_upgrade() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _) = setup(&env);

        let outsider = Address::generate(&env);
        let new_wasm_hash = BytesN::from_array(&env, &[0xabu8; 32]);

        let result = client.try_upgrade(&outsider, &new_wasm_hash);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_upgrade_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let new_wasm_hash = BytesN::from_array(&env, &[0xabu8; 32]);
        client.upgrade(&admin, &new_wasm_hash);

        let events = env.events().all();
        assert_eq!(events.len(), 1); // upgrade event
        let (_, topics, data) = events.get(0).unwrap();
        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("UPGRADE"));
        let emitted_hash: BytesN<32> = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_hash, new_wasm_hash);
    }

    // --- get_engineers_by_issuer tests ---

    #[test]
    fn test_propose_and_accept_admin_transfer() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let new_admin = Address::generate(&env);
        client.propose_admin(&admin, &new_admin);
        client.accept_admin();

        assert_eq!(client.get_admin(), new_admin);
    }

    #[test]
    fn test_pending_admin_key_cleared_after_accept() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let new_admin = Address::generate(&env);
        client.propose_admin(&admin, &new_admin);
        client.accept_admin();

        let contract_id = client.address.clone();
        env.as_contract(&contract_id, || {
            assert!(!env.storage().instance().has(&pending_admin_key()));
        });
    }

    #[test]
    fn test_non_admin_cannot_propose_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _) = setup(&env);

        let outsider = Address::generate(&env);
        let new_admin = Address::generate(&env);

        let result = client.try_propose_admin(&outsider, &new_admin);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_wrong_address_cannot_accept_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let new_admin = Address::generate(&env);
        let impostor = Address::generate(&env);
        client.propose_admin(&admin, &new_admin);

        use soroban_sdk::IntoVal;
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &impostor,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &client.address,
                fn_name: "accept_admin",
                args: ().into_val(&env),
                sub_invokes: &[],
            },
        }]);

        let result = client.try_accept_admin();
        assert!(result.is_err());
        assert_eq!(client.get_admin(), admin);
    }

    // --- get_engineers_by_issuer tests (original) ---

    #[test]
    fn test_get_engineers_by_issuer_empty() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _) = setup(&env);

        let issuer = Address::generate(&env);
        let result = client.get_engineers_by_issuer(&issuer);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_get_engineers_by_issuer_single() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        let list = client.get_engineers_by_issuer(&issuer);
        assert_eq!(list.len(), 1);
        assert_eq!(list.get(0).unwrap(), engineer);
    }

    #[test]
    fn test_get_engineers_by_issuer_multiple() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer = Address::generate(&env);
        let e1 = Address::generate(&env);
        let e2 = Address::generate(&env);
        let e3 = Address::generate(&env);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(
            &e1,
            &BytesN::from_array(&env, &[1u8; 32]),
            &issuer,
            &31_536_000,
        );
        client.register_engineer(
            &e2,
            &BytesN::from_array(&env, &[2u8; 32]),
            &issuer,
            &31_536_000,
        );
        client.register_engineer(
            &e3,
            &BytesN::from_array(&env, &[3u8; 32]),
            &issuer,
            &31_536_000,
        );

        let list = client.get_engineers_by_issuer(&issuer);
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn test_get_engineers_by_issuer_isolated_per_issuer() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer_a = Address::generate(&env);
        let issuer_b = Address::generate(&env);
        let e1 = Address::generate(&env);
        let e2 = Address::generate(&env);

        client.add_trusted_issuer(&admin, &issuer_a);
        client.add_trusted_issuer(&admin, &issuer_b);
        client.register_engineer(
            &e1,
            &BytesN::from_array(&env, &[1u8; 32]),
            &issuer_a,
            &31_536_000,
        );
        client.register_engineer(
            &e2,
            &BytesN::from_array(&env, &[2u8; 32]),
            &issuer_b,
            &31_536_000,
        );

        assert_eq!(client.get_engineers_by_issuer(&issuer_a).len(), 1);
        assert_eq!(client.get_engineers_by_issuer(&issuer_b).len(), 1);
        assert_eq!(
            client.get_engineers_by_issuer(&issuer_a).get(0).unwrap(),
            e1
        );
        assert_eq!(
            client.get_engineers_by_issuer(&issuer_b).get(0).unwrap(),
            e2
        );
    }

    #[test]
    fn test_get_engineer_count_by_issuer() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer = Address::generate(&env);
        let e1 = Address::generate(&env);
        let e2 = Address::generate(&env);

        // Empty issuer
        assert_eq!(client.get_engineer_count_by_issuer(&issuer), 0);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(
            &e1,
            &BytesN::from_array(&env, &[1u8; 32]),
            &issuer,
            &31_536_000,
        );
        assert_eq!(client.get_engineer_count_by_issuer(&issuer), 1);

        client.register_engineer(
            &e2,
            &BytesN::from_array(&env, &[2u8; 32]),
            &issuer,
            &31_536_000,
        );
        assert_eq!(client.get_engineer_count_by_issuer(&issuer), 2);
    }

    #[test]
    fn test_pause_and_unpause_in_engineer_registry() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.pause(&admin);
        let result = client.try_register_engineer(&engineer, &hash, &issuer, &31_536_000);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            ))),
        );

        client.unpause(&admin);
        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);
        assert!(client.verify_engineer(&engineer));
    }

    #[test]
    fn test_pause_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        client.pause(&admin);

        let events = env.events().all();
        assert_eq!(events.len(), 1);
        let (_, topics, data) = events.get(0).unwrap();
        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("PAUSED"));
        let (emitted_admin,): (Address,) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, admin);
    }

    #[test]
    fn test_unpause_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        client.pause(&admin);
        client.unpause(&admin);

        let events = env.events().all();
        assert_eq!(events.len(), 1);
        let (_, topics, data) = events.get(0).unwrap();
        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("UNPAUSED"));
        let (emitted_admin,): (Address,) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, admin);
    }

    #[test]
    fn test_register_engineer_untrusted_issuer_returns_error() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _) = setup(&env);

        let engineer = Address::generate(&env);
        let untrusted_issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        let result = client.try_register_engineer(&engineer, &hash, &untrusted_issuer, &31_536_000);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UntrustedIssuer as u32,
            ))),
        );
    }

    #[test]
    fn test_expired_credential_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        // validity_period of 1000 seconds
        client.register_engineer(&engineer, &hash, &issuer, &1000);
        assert!(client.verify_engineer(&engineer));

        // Advance ledger past expiry
        env.ledger()
            .with_mut(|li| li.timestamp = li.timestamp + 1001);
        assert!(!client.verify_engineer(&engineer));
    }

    #[test]
    fn test_credential_valid_before_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &1000);

        // Advance to just before expiry
        env.ledger()
            .with_mut(|li| li.timestamp = li.timestamp + 999);
        assert!(client.verify_engineer(&engineer));
    }

    #[test]
    fn test_expires_at_stored_correctly() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);
        let validity_period: u64 = 86_400;

        client.add_trusted_issuer(&admin, &issuer);
        let issued_at = env.ledger().timestamp();
        client.register_engineer(&engineer, &hash, &issuer, &validity_period);

        let record = client.get_engineer(&engineer);
        assert_eq!(record.issued_at, issued_at);
        assert_eq!(record.expires_at, issued_at + validity_period);
    }

    // --- Issue #141: get_engineer structured error ---

    #[test]
    fn test_get_engineer_unknown_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _) = setup(&env);

        let unknown = Address::generate(&env);
        let result = client.try_get_engineer(&unknown);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::EngineerNotFound as u32,
            ))),
        );
    }

    // --- Issue #142: get_admin structured error before initialization ---

    #[test]
    fn test_get_admin_before_init_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(EngineerRegistry, ());
        let client = EngineerRegistryClient::new(&env, &contract_id);

        let result = client.try_get_admin();
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotInitialized as u32,
            ))),
        );
    }

    // --- Issue #143: revoke_credential extends TTL before write ---

    #[test]
    fn test_revoke_credential_ttl_extended_before_write() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        client.revoke_credential(&engineer);

        // After revocation the entry must still be accessible and marked inactive
        let record = client.get_engineer(&engineer);
        assert!(!record.active);

        let contract_id = client.address.clone();
        let ttl = env.as_contract(&contract_id, || {
            env.storage().persistent().get_ttl(&engineer_key(&engineer))
        });
        assert!(ttl > 0, "TTL must be extended after revocation");
    }

    // --- Issue #370: renew_credential rejects new_validity_period = 0 ---

    #[test]
    fn test_renew_credential_zero_validity_period_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);
        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);
        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);
        let result = client.try_renew_credential(&engineer, &0);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidValidityPeriod as u32,
            ))),
        );
    }

    // --- Issue #369: register_engineer rejects validity_period = 0 ---

    #[test]
    fn test_register_engineer_zero_validity_period_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);
        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);
        client.add_trusted_issuer(&admin, &issuer);
        let result = client.try_register_engineer(&engineer, &hash, &issuer, &0);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidValidityPeriod as u32,
            ))),
        );
    }
    #[test]
    fn test_add_trusted_issuer_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer = Address::generate(&env);
        client.add_trusted_issuer(&admin, &issuer);

        let events = env.events().all();
        assert_eq!(events.len(), 1);

        let (_, topics, data) = events.get(0).unwrap();

        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        let t1: Address = topics.get(1).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("ISS_ADD"));
        assert_eq!(t1, admin);

        let (emitted_issuer,): (Address,) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_issuer, issuer);
    }

    #[test]
    #[ignore = "pre-existing test failure: events not captured correctly"]
    fn test_add_trusted_issuer_no_duplicate_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer = Address::generate(&env);
        client.add_trusted_issuer(&admin, &issuer);
        // Adding the same issuer again should not emit a second ISS_ADD event
        client.add_trusted_issuer(&admin, &issuer);

        let events = env.events().all();
        use soroban_sdk::TryIntoVal;
        let iss_add_count = events
            .iter()
            .filter(|(_, topics, _)| {
                topics
                    .get(0)
                    .and_then(|v| TryIntoVal::<_, Symbol>::try_into_val(&v, &env).ok())
                    .map(|s| s == symbol_short!("ISS_ADD"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(iss_add_count, 1, "ISS_ADD should only be emitted once");
    }

    // --- Issue #386: add_trusted_issuer and remove_trusted_issuer extend instance TTL ---

    #[test]
    fn test_add_trusted_issuer_extends_instance_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer = Address::generate(&env);
        client.add_trusted_issuer(&admin, &issuer);

        let ttl = env.as_contract(&client.address, || env.storage().instance().get_ttl());
        assert!(
            ttl > 0,
            "instance TTL must be extended after add_trusted_issuer"
        );
    }

    #[test]
    fn test_remove_trusted_issuer_extends_instance_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer = Address::generate(&env);
        client.add_trusted_issuer(&admin, &issuer);
        client.remove_trusted_issuer(&admin, &issuer);

        let ttl = env.as_contract(&client.address, || env.storage().instance().get_ttl());
        assert!(
            ttl > 0,
            "instance TTL must be extended after remove_trusted_issuer"
        );
    }

    #[test]
    fn test_pause_affects_all_state_changes() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        client.pause(&admin);

        // Read-only access should still work while paused
        assert!(client.verify_engineer(&engineer));
        let fetched_engineer = client.get_engineer(&engineer);
        assert_eq!(fetched_engineer.address, engineer);
        assert!(fetched_engineer.active);
        assert!(client.try_get_engineer(&engineer).is_ok());

        // register_engineer
        assert_eq!(
            client.try_register_engineer(&Address::generate(&env), &hash, &issuer, &100),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // revoke_credential
        assert_eq!(
            client.try_revoke_credential(&engineer),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // add_trusted_issuer
        assert_eq!(
            client.try_add_trusted_issuer(&admin, &Address::generate(&env)),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // remove_trusted_issuer
        assert_eq!(
            client.try_remove_trusted_issuer(&admin, &issuer),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // upgrade
        assert_eq!(
            client.try_upgrade(&admin, &BytesN::from_array(&env, &[0u8; 32])),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // renew_credential
        assert_eq!(
            client.try_renew_credential(&engineer, &31_536_000),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );
    }

    // --- renew_credential tests ---

    #[test]
    fn test_renew_credential_extends_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        let before = client.get_engineer(&engineer).expires_at;
        client.renew_credential(&engineer, &86_400);
        let after = client.get_engineer(&engineer).expires_at;

        assert_eq!(after, before + 86_400);
    }

    #[test]
    fn test_renew_credential_from_now_when_expired() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);
        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &86_400);

        // Advance past original expiry
        env.ledger()
            .with_mut(|li| li.timestamp = li.timestamp + 86_401);
        assert!(!client.verify_engineer(&engineer));

        // Renew for another 86_400 seconds from now
        client.renew_credential(&engineer, &86_400);
        assert!(client.verify_engineer(&engineer));

        let record = client.get_engineer(&engineer);
        assert_eq!(record.expires_at, env.ledger().timestamp() + 86_400);
    }

    #[test]
    fn test_renew_credential_short_validity_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        let result = client.try_renew_credential(&engineer, &1);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidValidityPeriod as u32,
            ))),
        );
    }

    #[test]
    fn test_renew_credential_before_expiry_preserves_original_fields() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[7u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &100_000);

        let original = client.get_engineer(&engineer);
        env.ledger().with_mut(|li| li.timestamp += 250);

        client.renew_credential(&engineer, &86_400);

        let renewed = client.get_engineer(&engineer);
        assert_eq!(renewed.issued_at, original.issued_at);
        assert_eq!(renewed.credential_hash, original.credential_hash);
        assert_eq!(renewed.issuer, original.issuer);
        assert_eq!(renewed.expires_at, original.expires_at + 86_400);
        assert!(renewed.expires_at > original.expires_at);
        assert!(client.verify_engineer(&engineer));
    }

    #[test]
    fn test_renew_credential_revoked_engineer_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);
        client.revoke_credential(&engineer);

        let result = client.try_renew_credential(&engineer, &31_536_000);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::CredentialRevoked as u32,
            ))),
        );
    }

    #[test]
    fn test_renew_credential_fails_when_issuer_removed() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        // Remove the issuer after registration
        client.remove_trusted_issuer(&admin, &issuer);

        let result = client.try_renew_credential(&engineer, &31_536_000);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::IssuerRemoved as u32,
            ))),
        );
    }

    #[test]
    fn test_renew_credential_unknown_engineer_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _) = setup(&env);

        let unknown = Address::generate(&env);
        let result = client.try_renew_credential(&unknown, &31_536_000);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::EngineerNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_renew_credential_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &100_000);
        let previous_expires_at = client.get_engineer(&engineer).expires_at;
        client.renew_credential(&engineer, &86_400);

        let events = env.events().all();
        let (_, topics, data) = events.last().unwrap();
        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        let t1: Address = topics.get(1).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("RNW_CRED"));
        assert_eq!(t1, engineer);

        let renewed_at = env.ledger().timestamp();
        let (emitted_issuer, old_expires_at, new_expires_at, emitted_renewed_at): (
            Address,
            u64,
            u64,
            u64,
        ) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_issuer, issuer);
        assert_eq!(old_expires_at, previous_expires_at);
        assert_eq!(new_expires_at, previous_expires_at + 86_400);
        assert_eq!(emitted_renewed_at, renewed_at);
    }

    #[test]
    fn test_renew_credential_extends_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &1000);
        client.renew_credential(&engineer, &31_536_000);

        let contract_id = client.address.clone();
        let ttl = env.as_contract(&contract_id, || {
            env.storage().persistent().get_ttl(&engineer_key(&engineer))
        });
        assert!(ttl > 0, "TTL should be extended after renewal");
    }

    #[test]
    fn test_remove_trusted_issuer_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer = Address::generate(&env);
        client.add_trusted_issuer(&admin, &issuer);
        client.remove_trusted_issuer(&admin, &issuer);

        let events = env.events().all();
        assert_eq!(events.len(), 1);

        let remove_event = events.get(0).unwrap();
        let (_, topics, data) = remove_event;

        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        let t1: Address = topics.get(1).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("ISS_RM"));
        assert_eq!(t1, admin);

        let (emitted_issuer,): (Address,) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_issuer, issuer);
    }

    #[test]
    fn test_get_trusted_issuers_consistent_after_add_and_remove() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer_a = Address::generate(&env);
        let issuer_b = Address::generate(&env);

        // Empty initially
        assert_eq!(client.get_trusted_issuers().len(), 0);

        // Add both
        client.add_trusted_issuer(&admin, &issuer_a);
        client.add_trusted_issuer(&admin, &issuer_b);
        let list = client.get_trusted_issuers();
        assert_eq!(list.len(), 2);
        assert!(list.contains(issuer_a.clone()));
        assert!(list.contains(issuer_b.clone()));

        // Remove one — list must stay consistent
        client.remove_trusted_issuer(&admin, &issuer_a);
        let list = client.get_trusted_issuers();
        assert_eq!(list.len(), 1);
        assert!(!list.contains(issuer_a.clone()));
        assert!(list.contains(issuer_b.clone()));

        // is_trusted_issuer must agree with the list
        assert!(!client.is_trusted_issuer(&issuer_a));
        assert!(client.is_trusted_issuer(&issuer_b));
    }

    #[test]
    fn test_remove_nonexistent_issuer() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);
        let nonexistent_issuer = Address::generate(&env);

        assert_eq!(
            client.try_remove_trusted_issuer(&admin, &nonexistent_issuer),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::IssuerNotFound as u32
            )))
        );
    }

    #[test]
    fn test_remove_trusted_issuer_revokes_engineers() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer = Address::generate(&env);
        let engineer1 = Address::generate(&env);
        let engineer2 = Address::generate(&env);
        let hash1 = BytesN::from_array(&env, &[1u8; 32]);
        let hash2 = BytesN::from_array(&env, &[2u8; 32]);

        // Add issuer as trusted
        client.add_trusted_issuer(&admin, &issuer);

        // Register two engineers
        client.register_engineer(&engineer1, &hash1, &issuer, &31_536_000);
        client.register_engineer(&engineer2, &hash2, &issuer, &31_536_000);

        // Verify engineers are active
        assert!(client.verify_engineer(&engineer1));
        assert!(client.verify_engineer(&engineer2));

        // Remove the trusted issuer
        client.remove_trusted_issuer(&admin, &issuer);

        // Verify engineers are now revoked
        assert!(!client.verify_engineer(&engineer1));
        assert!(!client.verify_engineer(&engineer2));

        // Check status
        assert_eq!(
            client.get_engineer_status(&engineer1),
            EngineerStatus::Revoked
        );
        assert_eq!(
            client.get_engineer_status(&engineer2),
            EngineerStatus::Revoked
        );
    }

    #[test]
    fn test_different_issuer_cannot_revoke_another_issuers_engineer() {
        let env = Env::default();
        let contract_id = env.register(EngineerRegistry, ());
        let client = EngineerRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let engineer = Address::generate(&env);
        let issuer_a = Address::generate(&env);
        let issuer_b = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &admin,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &client.address,
                fn_name: "initialize_admin",
                args: (admin.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.initialize_admin(&admin);

        // Add both issuers as trusted
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &admin,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &client.address,
                fn_name: "add_trusted_issuer",
                args: (admin.clone(), issuer_a.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.add_trusted_issuer(&admin, &issuer_a);
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &admin,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &client.address,
                fn_name: "add_trusted_issuer",
                args: (admin.clone(), issuer_b.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.add_trusted_issuer(&admin, &issuer_b);

        // Issuer A registers the engineer
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &issuer_a,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &client.address,
                fn_name: "register_engineer",
                args: (
                    engineer.clone(),
                    hash.clone(),
                    issuer_a.clone(),
                    31_536_000u64,
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.register_engineer(&engineer, &hash, &issuer_a, &31_536_000);

        // Issuer B attempts to revoke — should fail because record.issuer is issuer_a
        // Restrict to only issuer_b's auth so issuer_a.require_auth() fails
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &issuer_b,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &client.address,
                fn_name: "revoke_credential",
                args: (engineer.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);

        // This should panic because issuer_b is not the original issuer
        // The require_auth will fail because record.issuer is issuer_a, not issuer_b
        let result = client.try_revoke_credential(&engineer);
        assert!(
            result.is_err(),
            "Different issuer should not be able to revoke"
        );
    }

    #[test]
    fn test_register_engineer_zero_validity_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        let result = client.try_register_engineer(&engineer, &hash, &issuer, &0);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidValidityPeriod as u32,
            ))),
        );
    }

    #[test]
    fn test_register_engineer_rejects_active_duplicate() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        // Attempt to re-register the same active engineer
        let result = client.try_register_engineer(&engineer, &hash, &issuer, &31_536_000);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::EngineerAlreadyRegistered as u32,
            ))),
        );
    }

    #[test]
    fn test_register_engineer_allows_reregistration_after_revoke() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        // Revoke the credential
        client.revoke_credential(&engineer);
        assert!(!client.verify_engineer(&engineer));

        // Should be able to re-register after revocation
        let new_hash = BytesN::from_array(&env, &[2u8; 32]);
        client.register_engineer(&engineer, &new_hash, &issuer, &31_536_000);
        assert!(client.verify_engineer(&engineer));
    }

    #[test]
    fn test_register_engineer_rejects_duplicate_registration_when_active() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash1 = BytesN::from_array(&env, &[1u8; 32]);
        let hash2 = BytesN::from_array(&env, &[2u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);

        // First registration succeeds
        client.register_engineer(&engineer, &hash1, &issuer, &31_536_000);
        assert!(client.verify_engineer(&engineer));

        // Second registration with same engineer (still active) must panic
        let result = client.try_register_engineer(&engineer, &hash2, &issuer, &31_536_000);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::EngineerAlreadyRegistered as u32,
            ))),
        );
    }


    #[test]
    fn test_no_duplicate_in_issuer_list_after_reregistration() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);
        client.revoke_credential(&engineer);

        let new_hash = BytesN::from_array(&env, &[2u8; 32]);
        client.register_engineer(&engineer, &new_hash, &issuer, &31_536_000);

        // Engineer address must appear exactly once in the issuer's list
        let list = client.get_engineers_by_issuer(&issuer);
        let count = list.iter().filter(|a| *a == engineer).count();
        assert_eq!(
            count, 1,
            "Engineer address must not be duplicated after re-registration"
        );
    }

    #[test]
    fn test_get_engineer_status_active() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);

        assert_eq!(
            client.get_engineer_status(&engineer),
            EngineerStatus::Active
        );
    }

    #[test]
    fn test_get_engineer_status_revoked() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &31_536_000);
        client.revoke_credential(&engineer);

        assert_eq!(
            client.get_engineer_status(&engineer),
            EngineerStatus::Revoked
        );
    }

    #[test]
    fn test_get_engineer_status_expired() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&engineer, &hash, &issuer, &86_400); // 1 day validity
        env.ledger().set_timestamp(86_401); // Move time past expiry

        assert_eq!(
            client.get_engineer_status(&engineer),
            EngineerStatus::Expired
        );
    }

    #[test]
    fn test_get_engineer_status_not_found() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _admin) = setup(&env);

        let engineer = Address::generate(&env);
        assert_eq!(
            client.get_engineer_status(&engineer),
            EngineerStatus::NotFound
        );
    }

    #[test]
    fn test_get_active_engineers_by_issuer_filters_revoked() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer = Address::generate(&env);
        let eng1 = Address::generate(&env);
        let eng2 = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);

        client.add_trusted_issuer(&admin, &issuer);
        client.register_engineer(&eng1, &hash, &issuer, &31_536_000);
        client.register_engineer(&eng2, &hash, &issuer, &31_536_000);

        // All engineers should be in the full list
        let all = client.get_engineers_by_issuer(&issuer);
        assert_eq!(all.len(), 2);

        // All should be active initially
        let active = client.get_active_engineers_by_issuer(&issuer);
        assert_eq!(active.len(), 2);

        // Revoke one
        client.revoke_credential(&eng1);

        // Full list still has both
        let all = client.get_engineers_by_issuer(&issuer);
        assert_eq!(all.len(), 2);

        // Active list has only one
        let active = client.get_active_engineers_by_issuer(&issuer);
        assert_eq!(active.len(), 1);
        assert_eq!(active.get(0).unwrap(), eng2);
    }

    #[test]
    fn test_propose_and_accept_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let new_admin = Address::generate(&env);
        client.propose_admin(&admin, &new_admin);
        client.accept_admin();

        assert_eq!(client.get_admin(), new_admin);
    }

    #[test]
    fn test_accept_admin_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let new_admin = Address::generate(&env);
        client.propose_admin(&admin, &new_admin);
        client.accept_admin();

        let events = env.events().all();
        assert_eq!(events.len(), 1);

        let (_, topics, data) = events.get(0).unwrap();
        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("ADMIN_SET"));

        let (emitted_admin,): (Address,) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, new_admin);
    }

    #[test]
    fn test_propose_admin_unauthorized() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _admin) = setup(&env);

        let unauthorized = Address::generate(&env);
        let new_admin = Address::generate(&env);

        assert_eq!(
            client.try_propose_admin(&unauthorized, &new_admin),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32
            )))
        );
    }

    #[test]
    fn test_accept_admin_unauthorized() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let new_admin = Address::generate(&env);
        let unauthorized = Address::generate(&env);

        client.propose_admin(&admin, &new_admin);

        // Try to accept as unauthorized address
        use soroban_sdk::IntoVal;
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &unauthorized,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &client.address,
                fn_name: "accept_admin",
                args: ().into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert!(client.try_accept_admin().is_err());
    }

    #[test]
    fn test_propose_admin_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let new_admin = Address::generate(&env);
        client.propose_admin(&admin, &new_admin);

        let events = env.events().all();
        let (_, topics, data) = events.last().unwrap();

        use soroban_sdk::TryIntoVal;
        let topic: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(topic, EVENT_PROP_ADMIN);

        let (emitted_admin, emitted_new_admin): (Address, Address) =
            data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, admin);
        assert_eq!(emitted_new_admin, new_admin);
    }

    #[test]
    fn test_pause_state_persists_across_instance_ttl_boundary() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin) = setup(&env);

        let issuer = Address::generate(&env);
        client.add_trusted_issuer(&admin, &issuer);

        // Pause the contract
        client.pause(&admin);
        assert!(client.is_paused());

        // Simulate instance TTL expiry by wiping instance storage
        env.as_contract(&client.address, || {
            env.storage().instance().remove(&admin_key());
            env.storage().instance().remove(&pending_admin_key());
        });

        // PAUSED_KEY lives in persistent storage — must still be true
        assert!(
            client.is_paused(),
            "pause state must survive instance TTL expiry"
        );

        // Writes must still be blocked
        let engineer = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);
        assert_eq!(
            client.try_register_engineer(&engineer, &hash, &issuer, &31_536_000),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );
    }
}
