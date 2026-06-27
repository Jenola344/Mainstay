#![no_std]

mod errors;
mod scoring;
mod types;

use crate::errors::ContractError;
use crate::scoring::{apply_decay, compute_decay, get_task_weight, score_history_push};
use crate::types::{
    BatchRecord, Config, DataKey, MaintenanceRecord, ScoreEntry, TimelockProposal,
};
use shared::validation::require_non_empty_vec;
use soroban_sdk::{
    contract, contractimpl, panic_with_error, symbol_short, Address, BytesN, Env, String, Symbol,
    Vec,
};

const ASSET_REGISTRY: Symbol = symbol_short!("REGISTRY");
const ENG_REGISTRY: Symbol = symbol_short!("ENG_REG");
const CONFIG: Symbol = symbol_short!("CONFIG");
const PAUSED_KEY: Symbol = symbol_short!("PAUSED");
const PENDING_ADMIN_KEY: Symbol = symbol_short!("PADMIN");
const DEFAULT_MAX_HISTORY: u32 = 200;
const DEFAULT_SCORE_INCREMENT: u32 = 5;
const DEFAULT_DECAY_RATE: u32 = 5;
const DEFAULT_DECAY_INTERVAL: u64 = 2592000; // 30 days in seconds
const DEFAULT_ELIGIBILITY_THRESHOLD: u32 = 50;
const DEFAULT_MAX_NOTES_LENGTH: u32 = 256;
const TIMELOCK_DELAY_SECS: u64 = 48 * 60 * 60;
/// Minimum score returned for an asset that has at least one maintenance record.
/// Prevents decay from making a legitimately-maintained asset indistinguishable
/// from one with no history at all.
const MIN_SCORE_WITH_HISTORY: u32 = 1;
/// Maximum age in ledgers for maintenance record recency weighting.
/// Records older than this contribute zero to the collateral score.
/// 1 ledger ≈ 5 seconds → 518_400 ledgers ≈ 30 days.
/// Older records still contribute nothing, newer records are weighted linearly.
const MAX_AGE_LEDGERS: u64 = 518_400;

const EVENT_INIT: Symbol = symbol_short!("INIT");
const EVENT_MAINT: Symbol = symbol_short!("MAINT");
const EVENT_DECAY: Symbol = symbol_short!("DECAY");
const EVENT_REG_AST: Symbol = symbol_short!("REG_AST");
const EVENT_REG_ENG: Symbol = symbol_short!("REG_ENG");
const EVENT_RST_SCR: Symbol = symbol_short!("RST_SCR");
const EVENT_XFER: Symbol = symbol_short!("XFER");
const EVENT_PROP_ADMIN: Symbol = symbol_short!("PROP_ADM");
const EVENT_ADMIN_SET: Symbol = symbol_short!("ADMIN_SET");

/// Soroban persistent-storage TTL constants.
/// 1 ledger ≈ 5 seconds → 518_400 ledgers ≈ 30 days.
const TTL_THRESHOLD: u32 = 518_400;
const TTL_TARGET: u32 = 518_400;

fn history_key(asset_id: u64) -> (Symbol, u64) {
    (symbol_short!("HIST"), asset_id)
}

fn timelock_key(op: Symbol) -> (Symbol, Symbol) {
    (symbol_short!("TL_PROP"), op)
}

fn score_key(asset_id: u64) -> (Symbol, u64) {
    (symbol_short!("SCORE"), asset_id)
}

fn score_history_key(asset_id: u64) -> (Symbol, u64) {
    (symbol_short!("SCHIST"), asset_id)
}

fn last_update_key(asset_id: u64) -> (Symbol, u64) {
    (symbol_short!("LUPD"), asset_id)
}

fn engineer_history_key(engineer: &Address) -> (Symbol, Address) {
    (symbol_short!("ENG_HIST"), engineer.clone())
}

fn engineer_auth_key(asset_id: u64, engineer: &Address) -> (Symbol, u64, Address) {
    (symbol_short!("ENG_AUTH"), asset_id, engineer.clone())
}

fn frozen_key(asset_id: u64) -> (Symbol, u64) {
    (symbol_short!("FROZEN"), asset_id)
}

fn frozen_score_key(asset_id: u64) -> (Symbol, u64) {
    (symbol_short!("FRZ_SCR"), asset_id)
fn revoke_eng_timelock_key(asset_id: u64, engineer: &Address) -> (Symbol, u64, Address) {
    (symbol_short!("RVK_TL"), asset_id, engineer.clone())
}

fn require_engineer_authorized(env: &Env, asset_id: u64, engineer: &Address) {
    let authorized: bool = env
        .storage()
        .persistent()
        .get(&engineer_auth_key(asset_id, engineer))
        .unwrap_or(false);
    if !authorized {
        panic_with_error!(env, ContractError::EngineerNotAuthorized);
    }
}

fn engineer_history_add(env: &Env, engineer: &Address, asset_id: u64, max_history: u32) {
    let key = engineer_history_key(engineer);
    let mut ids: Vec<u64> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| Vec::new(env));

    // Check if asset_id already exists before appending
    let mut found = false;
    for id in ids.iter() {
        if id == asset_id {
            found = true;
            break;
        }
    }

    if !found {
        if max_history > 0 && ids.len() >= max_history {
            ids.remove(0);
        }
        ids.push_back(asset_id);
    }

    env.storage().persistent().set(&key, &ids);
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
}

fn engineer_history_remove(env: &Env, engineer: &Address, asset_id: u64) {
    let key = engineer_history_key(engineer);
    if let Some(ids) = env.storage().persistent().get::<_, Vec<u64>>(&key) {
        let mut new_ids: Vec<u64> = Vec::new(env);
        for id in ids.iter() {
            if id != asset_id {
                new_ids.push_back(id);
            }
        }
        if new_ids.len() < ids.len() {
            env.storage().persistent().set(&key, &new_ids);
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
        }
    }
}

fn get_asset_registry_addr(env: &Env) -> Address {
    env.storage()
        .persistent()
        .get(&ASSET_REGISTRY)
        .unwrap_or_else(|| panic_with_error!(env, ContractError::NotInitialized))
}

fn get_engineer_registry_addr(env: &Env) -> Address {
    env.storage()
        .persistent()
        .get(&ENG_REGISTRY)
        .unwrap_or_else(|| panic_with_error!(env, ContractError::NotInitialized))
}

fn set_asset_registry_addr(env: &Env, addr: &Address) {
    env.storage().persistent().set(&ASSET_REGISTRY, addr);
    env.storage()
        .persistent()
        .extend_ttl(&ASSET_REGISTRY, TTL_THRESHOLD, TTL_TARGET);
}

fn set_engineer_registry_addr(env: &Env, addr: &Address) {
    env.storage().persistent().set(&ENG_REGISTRY, addr);
    env.storage()
        .persistent()
        .extend_ttl(&ENG_REGISTRY, TTL_THRESHOLD, TTL_TARGET);
}

fn is_zero_address(env: &Env, addr: &Address) -> bool {
    *addr
        == Address::from_str(
            env,
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
        )
}

fn is_paused(env: &Env) -> bool {
    env.storage().persistent().get(&PAUSED_KEY).unwrap_or(false)
}

fn ensure_not_paused(env: &Env) {
    if is_paused(env) {
        panic_with_error!(env, ContractError::Paused);
    }
}

fn require_admin(env: &Env, admin: &Address) {
    admin.require_auth();
    let config: Config = env
        .storage()
        .persistent()
        .get(&CONFIG)
        .unwrap_or_else(|| panic_with_error!(env, ContractError::NotInitialized));
    if config.admin != *admin {
        panic_with_error!(env, ContractError::UnauthorizedAdmin);
    }
}

fn store_timelock(env: &Env, op: Symbol) {
    let key = timelock_key(op);
    env.storage().persistent().set(
        &key,
        &TimelockProposal {
            proposed_at: env.ledger().timestamp(),
            executed: false,
        },
    );
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
}

fn require_timelock_ready(env: &Env, op: Symbol) {
    let key = timelock_key(op);
    let mut proposal: TimelockProposal = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| panic_with_error!(env, ContractError::ProposalNotFound));
    if proposal.executed {
        panic_with_error!(env, ContractError::ProposalNotFound);
    }
    if env.ledger().timestamp().saturating_sub(proposal.proposed_at) < TIMELOCK_DELAY_SECS {
        panic_with_error!(env, ContractError::TimelockNotExpired);
    }
    proposal.executed = true;
    env.storage().persistent().set(&key, &proposal);
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
}

fn validate_notes_length(env: &Env, notes: &soroban_sdk::String, max: u32) {
    if notes.is_empty() {
        panic_with_error!(env, ContractError::NotesTooLong);
    }
    if notes.len() > max {
        panic_with_error!(env, ContractError::NotesTooLong);
    }
}

fn verify_asset_exists(env: &Env, asset_registry: &Address, asset_id: &u64) {
    let client = asset_registry::AssetRegistryClient::new(env, asset_registry);
    let result = client.try_get_asset(asset_id);
    if result.is_err() {
        panic_with_error!(env, ContractError::AssetNotFound);
    }
}

// Minimal client interface for cross-contract call to EngineerRegistry
mod engineer_registry {
    use soroban_sdk::{contractclient, Address, Env, Vec};

    #[allow(dead_code)]
    #[contractclient(name = "EngineerRegistryClient")]
    pub trait EngineerRegistry {
        fn verify_engineer(env: Env, engineer: Address) -> Option<bool>;
        fn batch_verify_engineers(env: Env, engineers: Vec<Address>) -> Vec<bool>;
    }
}

#[contract]
pub struct Lifecycle;

#[contractimpl]
impl Lifecycle {
    /// Propose a configuration change to the Lifecycle contract using the admin timelock.
    ///
    /// # Arguments
    /// * `admin` - The administrator requesting the config update
    /// * `op` - Symbol representing the update operation
    pub fn propose_config_update(env: Env, admin: Address, op: Symbol) {
        ensure_not_paused(&env);
        require_admin(&env, &admin);
        store_timelock(&env, op);
    }

    /// Execute a pending score increment update after the timelock expires.
    ///
    /// # Arguments
    /// * `admin` - The administrator performing the update
    /// * `score_increment` - New increment value for each maintenance task
    pub fn execute_update_score_increment(env: Env, admin: Address, score_increment: u32) {
        require_timelock_ready(&env, symbol_short!("SC_INC"));
        Self::update_score_increment(env, admin, score_increment);
    }

    /// Execute a pending decay configuration update after the timelock expires.
    ///
    /// # Arguments
    /// * `admin` - The administrator performing the update
    /// * `decay_rate` - Points to deduct per decay interval
    /// * `decay_interval` - Interval length in seconds used to compute decay
    pub fn execute_update_decay_config(
        env: Env,
        admin: Address,
        decay_rate: u32,
        decay_interval: u64,
    ) {
        require_timelock_ready(&env, symbol_short!("DEC_CFG"));
        Self::update_decay_config(env, admin, decay_rate, decay_interval);
    }

    /// Execute a pending eligibility threshold update after the timelock expires.
    ///
    /// # Arguments
    /// * `admin` - The administrator performing the update
    /// * `threshold` - New collateral eligibility threshold
    pub fn execute_update_eligibility(env: Env, admin: Address, threshold: u32) {
        require_timelock_ready(&env, symbol_short!("ELIG"));
        Self::update_eligibility_threshold(env, admin, threshold);
    }

    /// Execute a pending max history update after the timelock expires.
    ///
    /// # Arguments
    /// * `admin` - The administrator performing the update
    /// * `new_max` - Maximum maintenance history entries per asset
    pub fn execute_update_max_history(env: Env, admin: Address, new_max: u32) {
        require_timelock_ready(&env, symbol_short!("MAX_HIST"));
        Self::update_max_history(env, admin, new_max);
    }

    /// Execute a pending max notes length update after the timelock expires.
    ///
    /// # Arguments
    /// * `admin` - The administrator performing the update
    /// * `new_max` - Maximum length of maintenance notes
    pub fn execute_update_max_notes_length(env: Env, admin: Address, new_max: u32) {
        require_timelock_ready(&env, symbol_short!("MAX_NOTE"));
        Self::update_max_notes_length(env, admin, new_max);
    }

    /// Execute a pending asset registry address update after the timelock expires.
    ///
    /// # Arguments
    /// * `admin` - The administrator performing the update
    /// * `new_registry` - Address of the new asset registry contract
    pub fn execute_update_asset_registry(env: Env, admin: Address, new_registry: Address) {
        require_timelock_ready(&env, symbol_short!("AST_REG"));
        Self::update_asset_registry(env, admin, new_registry);
    }

    /// Execute a pending engineer registry address update after the timelock expires.
    ///
    /// # Arguments
    /// * `admin` - The administrator performing the update
    /// * `new_registry` - Address of the new engineer registry contract
    pub fn execute_update_engineer_registry(env: Env, admin: Address, new_registry: Address) {
        require_timelock_ready(&env, symbol_short!("ENG_REG"));
        Self::update_engineer_registry(env, admin, new_registry);
    }

    /// Owner-approved per-asset authorization for maintenance submissions.
    ///
    /// A verified engineer must also be explicitly authorized by the current asset owner
    /// before submitting maintenance for that asset.
    ///
    /// # Arguments
    /// * `owner` - The current owner of the asset
    /// * `asset_id` - The unique identifier of the asset
    /// * `engineer` - The engineer address being authorized
    ///
    /// # Panics
    /// - [`ContractError::UnauthorizedOwner`] if the caller is not the asset owner
    pub fn authorize_engineer(env: Env, owner: Address, asset_id: u64, engineer: Address) {
        ensure_not_paused(&env);
        owner.require_auth();

        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);
        let asset =
            asset_registry::AssetRegistryClient::new(&env, &asset_registry).get_asset(&asset_id);
        if asset.owner != owner {
            panic_with_error!(&env, ContractError::UnauthorizedOwner);
        }

        let key = engineer_auth_key(asset_id, &engineer);
        env.storage().persistent().set(&key, &true);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
    }

    /// Revoke an engineer's owner-approved authorization for a specific asset.
    ///
    /// Propose the revocation of an engineer's authorization for an asset.
    /// The revocation is subject to a timelock to give engineers a grace period
    /// to complete any in-progress maintenance work.
    ///
    /// # Arguments
    /// * `owner` - The current owner of the asset
    /// * `asset_id` - The unique identifier of the asset
    /// * `engineer` - The engineer address whose authorization is being revoked
    ///
    /// # Panics
    /// - [`ContractError::UnauthorizedOwner`] if the caller is not the asset owner
    pub fn propose_revoke_engineer_auth(env: Env, owner: Address, asset_id: u64, engineer: Address) {
        ensure_not_paused(&env);
        owner.require_auth();

        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);
        let asset =
            asset_registry::AssetRegistryClient::new(&env, &asset_registry).get_asset(&asset_id);
        if asset.owner != owner {
            panic_with_error!(&env, ContractError::UnauthorizedOwner);
        }

        let key = revoke_eng_timelock_key(asset_id, &engineer);
        env.storage().persistent().set(
            &key,
            &TimelockProposal {
                proposed_at: env.ledger().timestamp(),
                executed: false,
            },
        );
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);

        env.events().publish(
            (symbol_short!("PROP_RVK"), owner.clone()),
            (asset_id, engineer.clone(), env.ledger().timestamp()),
        );
    }

    /// Execute a previously proposed engineer authorization revocation after the timelock expires.
    ///
    /// # Arguments
    /// * `owner` - The current owner of the asset
    /// * `asset_id` - The unique identifier of the asset
    /// * `engineer` - The engineer address whose authorization is being revoked
    ///
    /// # Panics
    /// - [`ContractError::UnauthorizedOwner`] if the caller is not the asset owner
    /// - [`ContractError::ProposalNotFound`] if no revocation was proposed or already executed
    /// - [`ContractError::TimelockNotExpired`] if the delay has not elapsed
    pub fn execute_revoke_engineer_auth(env: Env, owner: Address, asset_id: u64, engineer: Address) {
        ensure_not_paused(&env);
        owner.require_auth();

        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);
        let asset =
            asset_registry::AssetRegistryClient::new(&env, &asset_registry).get_asset(&asset_id);
        if asset.owner != owner {
            panic_with_error!(&env, ContractError::UnauthorizedOwner);
        }

        let key = revoke_eng_timelock_key(asset_id, &engineer);
        let mut proposal: TimelockProposal = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::ProposalNotFound));
        if proposal.executed {
            panic_with_error!(&env, ContractError::ProposalNotFound);
        }
        if env.ledger().timestamp().saturating_sub(proposal.proposed_at) < TIMELOCK_DELAY_SECS {
            panic_with_error!(&env, ContractError::TimelockNotExpired);
        }
        proposal.executed = true;
        env.storage().persistent().set(&key, &proposal);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);

        env.storage()
            .persistent()
            .remove(&engineer_auth_key(asset_id, &engineer));

        env.events().publish(
            (symbol_short!("RVK_ENG"), owner.clone()),
            (asset_id, engineer.clone(), env.ledger().timestamp()),
        );
    }

    /// Clear all engineer authorizations for an asset after ownership transfer.
    /// Called by new owner to invalidate previous owner's engineer authorizations.
    ///
    /// # Arguments
    /// * `new_owner` - The current owner of the asset (must match registry)
    /// * `asset_id` - The asset whose engineer authorizations should be cleared
    ///
    /// # Panics
    /// - [`ContractError::UnauthorizedOwner`] if caller is not the current owner
    pub fn clear_engineer_authorizations(env: Env, new_owner: Address, asset_id: u64) {
        ensure_not_paused(&env);
        new_owner.require_auth();

        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);
        let asset =
            asset_registry::AssetRegistryClient::new(&env, &asset_registry).get_asset(&asset_id);
        if asset.owner != new_owner {
            panic_with_error!(&env, ContractError::UnauthorizedOwner);
        }

        // Get the maintenance history to find all engineers who have worked on this asset
        if let Some(history) = env
            .storage()
            .persistent()
            .get::<_, Vec<MaintenanceRecord>>(&history_key(asset_id))
        {
            let mut cleared_engineers = Vec::new(&env);
            for record in history.iter() {
                let eng = record.engineer.clone();
                let mut already_cleared = false;
                for cleared in cleared_engineers.iter() {
                    if cleared == eng {
                        already_cleared = true;
                        break;
                    }
                }
                if !already_cleared {
                    env.storage()
                        .persistent()
                        .remove(&engineer_auth_key(asset_id, &eng));
                    cleared_engineers.push_back(eng);
                }
            }
        }
    }

    /// Initialize the lifecycle contract with registry addresses and configuration.
    /// Must be called once after deployment to bind dependent registries.
    ///
    /// # Arguments
    /// * `asset_registry` - Address of the asset registry contract
    /// * `engineer_registry` - Address of the engineer registry contract
    /// * `deployer` - The address of the contract deployer; must sign this transaction.
    /// * `admin` - Address that will have administrative privileges
    /// * `max_history` - Maximum maintenance records per asset (0 for default 200)
    ///
    /// # Panics
    /// - [`ContractError::AlreadyInitialized`] if contract has already been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if deployer is not the transaction invoker
    pub fn initialize(
        env: Env,
        deployer: Address,
        asset_registry: Address,
        engineer_registry: Address,
        admin: Address,
        max_history: u32,
    ) {
        if deployer != env.invoker() {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        deployer.require_auth();
        if env.storage().persistent().has(&CONFIG) {
            panic_with_error!(&env, ContractError::AlreadyInitialized);
        }
        if max_history > 10_000 {
            panic_with_error!(&env, ContractError::InvalidConfig);
        }
        if asset_registry == engineer_registry {
            panic_with_error!(&env, ContractError::InvalidConfig);
        }

        set_asset_registry_addr(&env, &asset_registry);
        set_engineer_registry_addr(&env, &engineer_registry);

        let config = Config {
            admin: admin.clone(),
            max_history: if max_history == 0 {
                DEFAULT_MAX_HISTORY
            } else {
                max_history
            },
            score_increment: DEFAULT_SCORE_INCREMENT,
            decay_rate: DEFAULT_DECAY_RATE,
            decay_interval: DEFAULT_DECAY_INTERVAL,
            eligibility_threshold: DEFAULT_ELIGIBILITY_THRESHOLD,
            max_notes_length: DEFAULT_MAX_NOTES_LENGTH,
        };
        env.storage().persistent().set(&CONFIG, &config);
        env.storage()
            .persistent()
            .extend_ttl(&CONFIG, TTL_THRESHOLD, TTL_TARGET);

        env.events()
            .publish((EVENT_INIT,), (asset_registry, engineer_registry, admin));
    }

    /// Admin-only function to pause the contract.
    ///
    /// # Arguments
    /// * `admin` - The address that must match the stored admin
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn pause(env: Env, admin: Address) {
        admin.require_auth();
        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        env.storage().persistent().set(&PAUSED_KEY, &true);
        env.storage()
            .persistent()
            .extend_ttl(&PAUSED_KEY, TTL_THRESHOLD, TTL_TARGET);
        env.events()
            .publish((symbol_short!("PAUSED"),), (admin.clone(),));
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("PAUSED")),
            (admin, env.ledger().timestamp()),
        );
    }

    /// Admin-only function to unpause the contract.
    ///
    /// # Arguments
    /// * `admin` - The address that must match the stored admin
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn unpause(env: Env, admin: Address) {
        admin.require_auth();
        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        env.storage().persistent().set(&PAUSED_KEY, &false);
        env.storage()
            .persistent()
            .extend_ttl(&PAUSED_KEY, TTL_THRESHOLD, TTL_TARGET);
        env.events()
            .publish((symbol_short!("UNPAUSED"),), (admin.clone(),));
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("UNPAUSED")),
            (admin, env.ledger().timestamp()),
        );
    }

    /// Check if the contract is currently paused.
    ///
    /// # Returns
    /// `true` if paused; `false` otherwise
    pub fn is_paused(env: Env) -> bool {
        is_paused(&env)
    }

    /// Propose a new admin address (step 1 of 2-step transfer).
    /// Only the current admin can propose a new admin.
    ///
    /// # Arguments
    /// * `admin` - The current admin address
    /// * `new_admin` - The address to propose as the new admin
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the current admin
    /// - [`ContractError::PendingAdminAlreadyExists`] if a pending admin already exists
    pub fn propose_admin(env: Env, admin: Address, new_admin: Address) {
        admin.require_auth();
        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        if env.storage().instance().has(&PENDING_ADMIN_KEY) {
            panic_with_error!(&env, ContractError::PendingAdminAlreadyExists);
        }
        env.storage().instance().set(&PENDING_ADMIN_KEY, &new_admin);
        env.events()
            .publish((EVENT_PROP_ADMIN,), (admin.clone(), new_admin.clone()));
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("PROP_ADM")),
            (admin, env.ledger().timestamp(), new_admin),
        );
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
            .get(&PENDING_ADMIN_KEY)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        pending_admin.require_auth();

        let mut config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        config.admin = pending_admin.clone();
        env.storage().persistent().set(&CONFIG, &config);
        env.storage()
            .persistent()
            .extend_ttl(&CONFIG, TTL_THRESHOLD, TTL_TARGET);
        env.storage().instance().remove(&PENDING_ADMIN_KEY);
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("ADMIN_SET")),
            (pending_admin.clone(), env.ledger().timestamp()),
        );
        env.events().publish((EVENT_ADMIN_SET,), (pending_admin,));
    }

    /// Admin-only function to update the score increment configuration.
    /// This controls how much scores increase per maintenance task.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored config admin
    /// * `score_increment` - New score increment value (must be > 0)
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    /// - [`ContractError::InvalidConfig`] if score_increment is 0
    pub fn update_score_increment(env: Env, admin: Address, score_increment: u32) {
        ensure_not_paused(&env);
        admin.require_auth();

        if score_increment == 0 {
            panic_with_error!(&env, ContractError::InvalidConfig);
        }

        let mut config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        let old_increment = config.score_increment;
        config.score_increment = score_increment;
        env.storage().persistent().set(&CONFIG, &config);
        env.storage()
            .persistent()
            .extend_ttl(&CONFIG, TTL_THRESHOLD, TTL_TARGET);
        env.events().publish(
            (symbol_short!("CFG_UPD"),),
            (old_increment, score_increment),
        );
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("CFG_UPD")),
            (
                admin,
                env.ledger().timestamp(),
                symbol_short!("SCORE_INC"),
                score_increment,
            ),
        );
    }

    /// Admin-only function to update the decay rate and interval for collateral score decay.
    /// This controls how quickly scores decrease over time without maintenance.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored config admin
    /// * `decay_rate` - Points to deduct per decay interval
    /// * `decay_interval` - Time interval in seconds for each decay step (must be > 0)
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    /// - [`ContractError::InvalidConfig`] if decay_interval is 0
    pub fn update_decay_config(env: Env, admin: Address, decay_rate: u32, decay_interval: u64) {
        ensure_not_paused(&env);
        admin.require_auth();

        if decay_rate == 0 || decay_interval == 0 {
            panic_with_error!(&env, ContractError::InvalidConfig);
        }

        let mut config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        let old_decay_rate = config.decay_rate;
        let old_decay_interval = config.decay_interval;
        config.decay_rate = decay_rate;
        config.decay_interval = decay_interval;

        env.events().publish(
            (symbol_short!("CFG_UPD"),),
            (
                old_decay_rate,
                decay_rate,
                old_decay_interval,
                decay_interval,
            ),
        );
        env.storage().persistent().set(&CONFIG, &config);
        env.storage()
            .persistent()
            .extend_ttl(&CONFIG, TTL_THRESHOLD, TTL_TARGET);
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("CFG_UPD")),
            (
                admin,
                env.ledger().timestamp(),
                symbol_short!("DECAY"),
                decay_rate,
                decay_interval,
            ),
        );
    }

    /// Admin-only function to update the eligibility threshold for collateral scoring.
    /// This sets the minimum score required for an asset to be eligible as collateral.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored config admin
    /// * `threshold` - New eligibility threshold value
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn update_eligibility_threshold(env: Env, admin: Address, threshold: u32) {
        ensure_not_paused(&env);
        admin.require_auth();

        let mut config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        if threshold == 0 {
            panic_with_error!(&env, ContractError::InvalidConfig);
        }

        let old_threshold = config.eligibility_threshold;
        config.eligibility_threshold = threshold;
        env.storage().persistent().set(&CONFIG, &config);
        env.storage()
            .persistent()
            .extend_ttl(&CONFIG, TTL_THRESHOLD, TTL_TARGET);
        env.events()
            .publish((symbol_short!("CFG_UPD"),), (old_threshold, threshold));
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("CFG_UPD")),
            (
                admin,
                env.ledger().timestamp(),
                symbol_short!("ELIG"),
                threshold,
            ),
        );
    }

    /// Admin-only function to update the maximum history records per asset.
    /// This allows adjusting the cap on maintenance history without redeployment.
    ///
    /// # Lazy Pruning Behavior
    /// When `new_max` is lower than the current cap, existing per-asset histories that exceed
    /// the new cap are **not** automatically pruned. Pruning happens lazily during the next
    /// maintenance submission for that asset. To immediately prune an asset's history to the
    /// new cap, use `prune_asset_history()`.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored config admin
    /// * `new_max` - New maximum history value (must be > 0)
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    /// - [`ContractError::InvalidConfig`] if new_max is 0
    pub fn update_max_history(env: Env, admin: Address, new_max: u32) {
        ensure_not_paused(&env);
        admin.require_auth();

        if new_max == 0 {
            panic_with_error!(&env, ContractError::InvalidConfig);
        }

        let mut config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        config.max_history = new_max;
        env.storage().persistent().set(&CONFIG, &config);
        env.storage()
            .persistent()
            .extend_ttl(&CONFIG, TTL_THRESHOLD, TTL_TARGET);

        env.events()
            .publish((symbol_short!("UPD_MAX"), admin.clone()), new_max);
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("CFG_UPD")),
            (
                admin,
                env.ledger().timestamp(),
                symbol_short!("MAX_HIST"),
                new_max,
            ),
        );
    }

    /// Admin-only function to update the maximum allowed notes length per maintenance record.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored config admin
    /// * `new_max` - New maximum notes length in bytes (must be > 0)
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    /// - [`ContractError::InvalidConfig`] if new_max is 0
    pub fn update_max_notes_length(env: Env, admin: Address, new_max: u32) {
        ensure_not_paused(&env);
        admin.require_auth();

        if new_max == 0 {
            panic_with_error!(&env, ContractError::InvalidConfig);
        }

        let mut config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        config.max_notes_length = new_max;
        env.storage().persistent().set(&CONFIG, &config);
        env.storage()
            .persistent()
            .extend_ttl(&CONFIG, TTL_THRESHOLD, TTL_TARGET);

        env.events()
            .publish((symbol_short!("UPD_NOTES"), admin.clone()), new_max);
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("CFG_UPD")),
            (
                admin,
                env.ledger().timestamp(),
                symbol_short!("MAX_NOTE"),
                new_max,
            ),
        );
    }
    /// Submit a maintenance record for an asset.
    /// Only verified engineers can submit maintenance records.
    ///
    /// # Ownership Transfer Note
    /// Maintenance history is asset-scoped and persists across ownership transfers.
    /// Records submitted before a transfer still reference the original engineer addresses.
    /// A sentinel record with `task_type = "XFER"` is inserted by [`record_transfer`] to
    /// mark the ownership boundary; records before it were performed under the previous owner.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset being maintained
    /// * `task_type` - Symbol representing the type of maintenance task
    /// * `notes` - String containing maintenance notes and details
    /// * `engineer` - Address of the engineer performing the maintenance
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    /// - [`ContractError::UnauthorizedEngineer`] if the engineer is not verified
    /// - [`ContractError::HistoryCapReached`] if the asset has reached max history records
    pub fn submit_maintenance(
        env: Env,
        asset_id: u64,
        task_type: Symbol,
        notes: String,
        engineer: Address,
    ) {
        ensure_not_paused(&env);
        engineer.require_auth();

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));

        // Validate task type early before cross-contract calls
        let _weight = get_task_weight(&env, &task_type);
        validate_notes_length(&env, &notes, config.max_notes_length);

        // Check history cap before cross-contract calls to avoid wasting gas
        let mut history: Vec<MaintenanceRecord> = env
            .storage()
            .persistent()
            .get(&history_key(asset_id))
            .unwrap_or(Vec::new(&env));

        if history.len() >= config.max_history {
            panic_with_error!(&env, ContractError::HistoryCapReached);
        }

        // Verify asset exists
        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);

        // Cross-check engineer credential via registry
        let registry_id = get_engineer_registry_addr(&env);
        let registry = engineer_registry::EngineerRegistryClient::new(&env, &registry_id);
        use engineer_registry::CredentialStatus;
        let status = registry.verify_engineer(&engineer);
        if status != CredentialStatus::Valid {
        let verified = registry.verify_engineer(&engineer).unwrap_or(false);
        if !verified {
            panic_with_error!(&env, ContractError::UnauthorizedEngineer);
        }
        require_engineer_authorized(&env, asset_id, &engineer);

        let timestamp = env.ledger().timestamp();

        let record = MaintenanceRecord {
            asset_id,
            task_type: task_type.clone(),
            notes,
            engineer: engineer.clone(),
            timestamp,
        };

        history.push_back(record);
        env.storage()
            .persistent()
            .set(&history_key(asset_id), &history);
        env.storage()
            .persistent()
            .extend_ttl(&history_key(asset_id), TTL_THRESHOLD, TTL_TARGET);

        engineer_history_add(&env, &engineer, asset_id, config.max_history);

        // Accumulate score: add this submission's increment to the stored score (cap at 100).
        let current_score: u32 = env
            .storage()
            .persistent()
            .get(&score_key(asset_id))
            .unwrap_or(0);
        let new_score = current_score.saturating_add(config.score_increment).min(100);

        // Persist the accumulated score so apply_decay / get_collateral_score can read it.
        env.storage().persistent().set(&score_key(asset_id), &new_score);
        env.storage().persistent().extend_ttl(&score_key(asset_id), TTL_THRESHOLD, TTL_TARGET);

        // Append (timestamp, score) snapshot to score history for historical tracking
        score_history_push(
            &env,
            asset_id,
            ScoreEntry {
                timestamp,
                score: new_score,
            },
            config.max_history,
        );

        // Update last maintenance timestamp for decay tracking
        env.storage()
            .persistent()
            .set(&last_update_key(asset_id), &timestamp);
        env.storage().persistent().extend_ttl(
            &last_update_key(asset_id),
            TTL_THRESHOLD,
            TTL_TARGET,
        );

        // Emit maintenance submission event
        env.events().publish(
            (symbol_short!("maint"),),
            (asset_id, engineer.clone(), task_type, timestamp),
        );
    }

    /// Record an ownership transfer in the asset's maintenance history.
    ///
    /// Appends a sentinel [`MaintenanceRecord`] with `task_type = "XFER"` and emits a
    /// `XFER` event so that indexers and new owners can identify the ownership boundary.
    /// Records before this sentinel were performed under the previous owner; records after
    /// it are performed under the new owner.
    /// Record a sentinel entry in the maintenance history to mark an ownership transfer.
    ///
    /// Must be called **after** `asset_registry::transfer_asset` has already updated the
    /// on-chain owner, and must be called by the new owner (i.e. the address that now owns
    /// the asset in the registry).  The function cross-calls the asset registry to confirm
    /// that `new_owner` is indeed the current owner before writing the sentinel, preventing
    /// a replay of `new_owner`'s signature from inserting a false transfer record into an
    /// asset they do not own.
    ///
    /// # Arguments
    /// * `asset_id`       - The unique identifier of the transferred asset
    /// * `previous_owner` - Address of the owner before the transfer
    /// * `new_owner`      - Address of the owner after the transfer (must match registry)
    ///
    /// # Events
    /// Emits `(EVENT_XFER, asset_id)` with data `(previous_owner, new_owner, timestamp, sentinel_index)`
    /// where `sentinel_index` is the zero-based position of the XFER sentinel in the history vec,
    /// allowing indexers to directly correlate the event with the record.
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    /// - [`ContractError::UnauthorizedOwner`] if `new_owner` does not match the current
    ///   owner recorded in the asset registry
    pub fn record_transfer(env: Env, asset_id: u64, previous_owner: Address, new_owner: Address) {
        ensure_not_paused(&env);
        new_owner.require_auth();

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));

        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);

        // Verify new_owner is actually the current owner in the asset registry.
        // This prevents a signature replay from inserting a false transfer sentinel
        // into an asset the caller does not own.
        let registry_client = asset_registry::AssetRegistryClient::new(&env, &asset_registry);
        let asset = registry_client.get_asset(&asset_id);
        if asset.owner != new_owner {
            panic_with_error!(&env, ContractError::UnauthorizedOwner);
        }

        let timestamp = env.ledger().timestamp();
        let sentinel = MaintenanceRecord {
            asset_id,
            task_type: symbol_short!("XFER"),
            notes: String::from_str(&env, "Ownership transferred"),
            engineer: new_owner.clone(),
            timestamp,
        };

        let mut history: Vec<MaintenanceRecord> = env
            .storage()
            .persistent()
            .get(&history_key(asset_id))
            .unwrap_or_else(|| Vec::new(&env));

        if config.max_history > 0 && history.len() >= config.max_history {
            history.remove(0);
        }
        history.push_back(sentinel);
        let sentinel_index = history.len() - 1;
        env.storage()
            .persistent()
            .set(&history_key(asset_id), &history);
        env.storage()
            .persistent()
            .extend_ttl(&history_key(asset_id), TTL_THRESHOLD, TTL_TARGET);

        env.events().publish(
            (EVENT_XFER, asset_id),
            (previous_owner, new_owner, timestamp, sentinel_index),
        );
    }

    /// Submit multiple maintenance records for the same asset in a single transaction.
    /// All records are validated before any are written to ensure atomicity.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset being maintained
    /// * `records` - Vec of BatchRecord containing maintenance data
    /// * `engineer` - Address of the engineer performing the maintenance
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    /// - [`ContractError::UnauthorizedEngineer`] if the engineer is not verified
    /// - [`ContractError::HistoryCapReached`] if adding records would exceed max history
    pub fn batch_submit_maintenance(
        env: Env,
        asset_id: u64,
        records: Vec<BatchRecord>,
        engineer: Address,
    ) {
        ensure_not_paused(&env);
        engineer.require_auth();

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));

        // Validate records early before cross-contract calls
        require_non_empty_vec(&records, "records");
        for record in records.iter() {
            validate_notes_length(&env, &record.notes, config.max_notes_length);
            // Validate task type is known
            let _ = get_task_weight(&env, &record.task_type);
        }

        // Validate asset exists
        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);

        // Validate engineer credential via batch call to reduce future round-trips.
        let engineer_registry = get_engineer_registry_addr(&env);
        let engineer_registry_client =
            engineer_registry::EngineerRegistryClient::new(&env, &engineer_registry);
        use engineer_registry::CredentialStatus;
        let status = engineer_registry_client.verify_engineer(&engineer);
        if status != CredentialStatus::Valid {
        let mut batch = Vec::new(&env);
        batch.push_back(engineer.clone());
        let results = engineer_registry_client.batch_verify_engineers(&batch);
        let verified = results.get(0).unwrap_or(false);
        if !verified {
            panic_with_error!(&env, ContractError::UnauthorizedEngineer);
        }
        require_engineer_authorized(&env, asset_id, &engineer);

        let timestamp = env.ledger().timestamp();
        let mut history: Vec<MaintenanceRecord> = env
            .storage()
            .persistent()
            .get(&history_key(asset_id))
            .unwrap_or(Vec::new(&env));

        // Validate all records fit before writing any
        if history.len() + records.len() > config.max_history {
            panic_with_error!(&env, ContractError::HistoryCapReached);
        }

        // Build all records and compute final score before any write.
        let mut score: u32 = env
            .storage()
            .persistent()
            .get(&score_key(asset_id))
            .unwrap_or(0u32);

        let mut new_records: Vec<MaintenanceRecord> = Vec::new(&env);
        let mut score_entries: Vec<ScoreEntry> = Vec::new(&env);
        for record in records.iter() {
            score = score
                .checked_add(config.score_increment)
                .map(|s| s.min(100))
                .unwrap_or_else(|| panic_with_error!(&env, ContractError::ScoreOverflow));
            new_records.push_back(MaintenanceRecord {
                asset_id,
                task_type: record.task_type.clone(),
                notes: record.notes.clone(),
                engineer: engineer.clone(),
                timestamp,
            });
            score_entries.push_back(ScoreEntry { timestamp, score });
        }

        // All validation passed — now commit everything atomically.
        for record in new_records.iter() {
            history.push_back(record);
        }
        for entry in score_entries.iter() {
            score_history_push(&env, asset_id, entry, config.max_history);
        }
        for record in records.iter() {
            env.events().publish(
                (EVENT_MAINT, asset_id),
                (record.task_type.clone(), engineer.clone(), timestamp),
            );
        }

        // Add to engineer history only once per asset per batch
        engineer_history_add(&env, &engineer, asset_id, config.max_history);

        env.storage()
            .persistent()
            .set(&history_key(asset_id), &history);
        env.storage()
            .persistent()
            .extend_ttl(&history_key(asset_id), TTL_THRESHOLD, TTL_TARGET);
        env.storage().persistent().set(&score_key(asset_id), &score);
        env.storage()
            .persistent()
            .extend_ttl(&score_key(asset_id), TTL_THRESHOLD, TTL_TARGET);
        env.storage()
            .persistent()
            .set(&last_update_key(asset_id), &timestamp);
        env.storage().persistent().extend_ttl(
            &last_update_key(asset_id),
            TTL_THRESHOLD,
            TTL_TARGET,
        );
    }

    /// Apply time-based decay to an asset's collateral score.
    /// Can be called by anyone to ensure scores reflect current maintenance status.
    /// Uses configured decay rate and interval settings.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset to decay
    ///
    /// # Returns
    /// The new collateral score after applying decay
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    pub fn decay_score(env: Env, asset_id: u64) -> u32 {
        ensure_not_paused(&env);
        // Frozen (decommissioned) assets do not decay; return the frozen score.
        if env.storage().persistent().get::<_, bool>(&frozen_key(asset_id)).unwrap_or(false) {
            return env
                .storage()
                .persistent()
                .get(&frozen_score_key(asset_id))
                .unwrap_or(0);
        }
        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        apply_decay(&env, asset_id, true, true, config.max_history)
    }

    /// Called by the asset registry when an asset is decommissioned.
    /// Captures the current collateral score and freezes it so that lenders
    /// see the final verified state rather than a decayed ghost score.
    ///
    /// Authorization: only callable by the stored asset registry address.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the decommissioned asset
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    pub fn decommission_notify(env: Env, asset_id: u64) {
        let asset_registry = get_asset_registry_addr(&env);
        asset_registry.require_auth();
        env.storage()
            .persistent()
            .get::<_, Config>(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));

        let frozen_score = compute_decay(&env, asset_id);
        env.storage()
            .persistent()
            .set(&frozen_score_key(asset_id), &frozen_score);
        env.storage()
            .persistent()
            .extend_ttl(&frozen_score_key(asset_id), TTL_THRESHOLD, TTL_TARGET);
        env.storage()
            .persistent()
            .set(&frozen_key(asset_id), &true);
        env.storage()
            .persistent()
            .extend_ttl(&frozen_key(asset_id), TTL_THRESHOLD, TTL_TARGET);

        env.events()
            .publish((symbol_short!("DECOMM"), asset_id), frozen_score);
    }

    /// Get the complete maintenance history for an asset.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset
    ///
    /// # Returns
    /// Vec containing all maintenance records in chronological order
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::AssetNotFound`] if no asset exists with the given ID
    pub fn get_maintenance_history(env: Env, asset_id: u64) -> Vec<MaintenanceRecord> {
        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);
        env.storage()
            .persistent()
            .get(&history_key(asset_id))
            .unwrap_or(Vec::new(&env))
    }

    /// Get a paginated slice of the maintenance history for an asset.
    /// Useful for UI components that display maintenance records in pages.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset
    /// * `offset` - Zero-based start index for pagination
    /// * `limit` - Maximum number of records to return (returns empty vec if 0)
    ///
    /// # Returns
    /// Vec containing the requested page of maintenance records
    ///
    /// # Panics
    /// - [`ContractError::IndexOutOfBounds`] if `offset` >= history length
    pub fn get_maintenance_history_page(
        env: Env,
        asset_id: u64,
        offset: u32,
        limit: u32,
    ) -> Vec<MaintenanceRecord> {
        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);

        let history: Vec<MaintenanceRecord> = env
            .storage()
            .persistent()
            .get(&history_key(asset_id))
            .unwrap_or(Vec::new(&env));

        let len = history.len();
        if limit == 0 {
            return Vec::new(&env);
        }
        if offset >= len {
            return Vec::new(&env);
        }

        let end = (offset + limit).min(len);
        let mut page = Vec::new(&env);
        for i in offset..end {
            page.push_back(history.get(i).unwrap());
        }
        page
    }

    /// Get the most recent maintenance record for an asset, determined by the highest timestamp.
    ///
    /// History is append-only (records are never inserted out of order by normal contract
    /// operations), but this function defensively selects the record with the greatest
    /// timestamp so that any future admin tooling that inserts records cannot silently
    /// return a stale entry.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset
    ///
    /// # Returns
    /// `Some(MaintenanceRecord)` with the highest timestamp, or `None` if no history exists
    pub fn get_last_service(env: Env, asset_id: u64) -> Option<MaintenanceRecord> {
        let history: Vec<MaintenanceRecord> =
            env.storage().persistent().get(&history_key(asset_id))?;

        let mut best: Option<MaintenanceRecord> = None;
        for i in 0..history.len() {
            let record = history.get(i).unwrap();
            let is_newer = best.as_ref().is_none_or(|b| record.timestamp > b.timestamp);
            if is_newer {
                best = Some(record);
            }
        }
        best
    }

    /// Get the current collateral score for an asset.
    /// Verifies asset exists before returning the score.
    ///
    /// This function is **read-only**: it computes the time-decayed score without
    /// writing anything to storage. To persist the decayed score and update the
    /// last-update timestamp, call [`decay_score`] explicitly.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset
    ///
    /// # Returns
    /// The current collateral score (0-100) after applying time-based decay
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    pub fn get_collateral_score(env: Env, asset_id: u64) -> u32 {
        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);
        let config: Config = env
            .storage()
            .persistent()
            .get::<_, Config>(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        // Frozen (decommissioned) assets return the score captured at decommission time.
        if env.storage().persistent().get::<_, bool>(&frozen_key(asset_id)).unwrap_or(false) {
            return env
                .storage()
                .persistent()
                .get(&frozen_score_key(asset_id))
                .unwrap_or(0);
        }
        // Compute score from maintenance history (recency-weighted, decreases as records age).
        let history_score = compute_decay(&env, asset_id);

        // Compute score from accumulated stored value with lazy config-based decay applied.
        // This is read-only — does NOT write back to storage.
        let config_score = {
            let stored: u32 = env
                .storage()
                .persistent()
                .get(&score_key(asset_id))
                .unwrap_or(0);
            let last_update: u64 = env
                .storage()
                .persistent()
                .get(&last_update_key(asset_id))
                .unwrap_or(0);
            let elapsed = env.ledger().timestamp().saturating_sub(last_update);
            let intervals = elapsed / config.decay_interval;
            let decay = (intervals as u32).saturating_mul(config.decay_rate);
            stored.saturating_sub(decay)
        };

        // Use the lower of the two scores so both models can constrain the result.
        let score = history_score.min(config_score);

        // Apply floor: an asset with at least one maintenance record always scores >= 1
        // so it is never indistinguishable from an asset with no history.
        let has_history = env
            .storage()
            .persistent()
            .get::<_, Vec<MaintenanceRecord>>(&history_key(asset_id))
            .map(|h| !h.is_empty())
            .unwrap_or(false);
        let final_score = if has_history && score < MIN_SCORE_WITH_HISTORY {
            MIN_SCORE_WITH_HISTORY
        } else {
            score
        };
        // Persist the computed score so stored and returned values are always consistent.
        env.storage()
            .persistent()
            .set(&score_key(asset_id), &final_score);
        env.storage()
            .persistent()
            .extend_ttl(&score_key(asset_id), TTL_THRESHOLD, TTL_TARGET);
        env.storage()
            .persistent()
            .set(&last_update_key(asset_id), &env.ledger().timestamp());
        env.storage()
            .persistent()
            .extend_ttl(&last_update_key(asset_id), TTL_THRESHOLD, TTL_TARGET);
        final_score
    }

    /// Get collateral scores for multiple assets in a single call.
    ///
    /// # Arguments
    /// * `asset_ids` - A list of asset IDs to query
    ///
    /// # Returns
    /// A Vec of `(asset_id, score)` pairs with lazy decay applied. Unknown asset
    /// IDs are skipped (omitted from results) rather than causing a panic.
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if the contract is not initialized
    pub fn get_collateral_score_batch(env: Env, asset_ids: Vec<u64>) -> Vec<(u64, u32)> {
        let config: Config = env
            .storage()
            .persistent()
            .get::<_, Config>(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        let asset_registry = get_asset_registry_addr(&env);
        let client = asset_registry::AssetRegistryClient::new(&env, &asset_registry);
        let mut results: Vec<(u64, u32)> = Vec::new(&env);
        for asset_id in asset_ids.iter() {
            if client.try_get_asset(&asset_id).is_err() {
                continue;
            }
            let score = apply_decay(&env, asset_id, false, false, config.max_history);
            results.push_back((asset_id, score));
        }
        results
    }

    /// Returns the full score trend: one (timestamp, score) entry per maintenance event.
    /// Get the complete score history for an asset.
    /// Returns one (timestamp, score) entry per maintenance event.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset
    ///
    /// # Returns
    /// Vec of ScoreEntry containing the complete score trend
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    pub fn get_score_history(env: Env, asset_id: u64) -> Vec<ScoreEntry> {
        env.storage()
            .persistent()
            .get(&score_history_key(asset_id))
            .unwrap_or(Vec::new(&env))
    }

    /// Get the last `n` ScoreEntry items from the score history.
    /// Useful for displaying recent score trends in dashboards.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset
    /// * `n` - Number of most recent entries to return
    ///
    /// # Returns
    /// Vec containing the last `n` score entries (or fewer if history is shorter)
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    pub fn get_score_trend(env: Env, asset_id: u64, n: u32) -> Vec<ScoreEntry> {
        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);
        if n == 0 {
            return Vec::new(&env);
        }
        let history: Vec<ScoreEntry> = env
            .storage()
            .persistent()
            .get(&score_history_key(asset_id))
            .unwrap_or(Vec::new(&env));
        let len = history.len();
        if len == 0 {
            return Vec::new(&env);
        }
        let start = if n >= len {
            0u32
        } else {
            len.saturating_sub(n)
        };
        let mut result = Vec::new(&env);
        for i in start..len {
            result.push_back(history.get(i).unwrap());
        }
        result
    }

    /// Check if an asset is eligible for collateral based on its score.
    /// Verifies asset exists and compares score to eligibility threshold.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset
    ///
    /// # Returns
    /// `true` if the asset meets eligibility criteria; `false` otherwise
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    pub fn is_collateral_eligible(env: Env, asset_id: u64) -> bool {
        // Verify asset exists before checking eligibility
        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));

        // Use read-only decay computation since we already verified asset exists
        let score = compute_decay(&env, asset_id);
        let has_history = env
            .storage()
            .persistent()
            .get::<_, Vec<MaintenanceRecord>>(&history_key(asset_id))
            .map(|h| !h.is_empty())
            .unwrap_or(false);
        let effective_score = if has_history && score < MIN_SCORE_WITH_HISTORY {
            MIN_SCORE_WITH_HISTORY
        } else {
            score
        };
        effective_score >= config.eligibility_threshold
    }

    /// Returns the timestamp of the most recent maintenance event, or None if no maintenance has been submitted.
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    pub fn get_last_service_timestamp(env: Env, asset_id: u64) -> Option<u64> {
        let asset_registry = get_asset_registry_addr(&env);
        verify_asset_exists(&env, &asset_registry, &asset_id);
        env.storage().persistent().get(&last_update_key(asset_id))
    }

    /// Get the address of the asset registry contract.
    ///
    /// # Returns
    /// The address of the currently configured asset registry
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    pub fn get_asset_registry(env: Env) -> Address {
        get_asset_registry_addr(&env)
    }

    /// Get all asset IDs that have been maintained by a specific engineer.
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer to query
    ///
    /// # Returns
    /// A Vec containing the **first 100** asset IDs this engineer has worked on.
    /// If the engineer has more than 100 entries the result is silently truncated —
    /// call [`get_eng_maint_hist_count`] to check the total, then use
    /// call [`get_eng_maint_count`] to check the total, then use
    /// [`get_eng_history_page`] with `offset`/`limit` to retrieve the full history.
    pub fn get_engineer_maintenance_history(env: Env, engineer: Address) -> Vec<u64> {
        let history: Vec<u64> = env
            .storage()
            .persistent()
            .get(&engineer_history_key(&engineer))
            .unwrap_or_else(|| Vec::new(&env));

        let len = history.len();
        if len <= 100 {
            return history;
        }

        let mut result = Vec::new(&env);
        for i in 0..100u32 {
            result.push_back(history.get(i).unwrap());
        }
        result
    }

    /// Return the total number of asset IDs recorded for an engineer.
    ///
    /// Use this together with [`get_eng_history_page`] to paginate through histories
    /// that exceed the 100-entry cap of [`get_engineer_maintenance_history`].
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer to query
    ///
    /// # Returns
    /// Total number of entries in the engineer's maintenance history.
    pub fn get_eng_maint_hist_count(env: Env, engineer: Address) -> u32 {
        let history: Vec<u64> = env
            .storage()
            .persistent()
            .get(&engineer_history_key(&engineer))
            .unwrap_or_else(|| Vec::new(&env));
        history.len()
    }

    /// Alias for [`get_eng_maint_hist_count`].
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer to query
    ///
    /// # Returns
    /// Total number of entries in the engineer's maintenance history.
    pub fn get_engineer_history_count(env: Env, engineer: Address) -> u32 {
        Self::get_eng_maint_hist_count(env, engineer)
    }

    /// Alias for [`get_eng_maint_hist_count`].
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer to query
    ///
    /// # Returns
    /// Total number of entries in the engineer's maintenance history.
    pub fn get_eng_maint_count(env: Env, engineer: Address) -> u32 {
        Self::get_eng_maint_hist_count(env, engineer)
    }

    /// Alias for [`get_eng_maint_hist_count`].
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer to query
    ///
    /// # Returns
    /// Total number of entries in the engineer's maintenance history.
    pub fn eng_maintenance_history_count(env: Env, engineer: Address) -> u32 {
        Self::get_eng_maint_hist_count(env, engineer)
    }

    /// Get a paginated list of asset IDs that an engineer has worked on.
    ///
    /// # Arguments
    /// * `engineer` - The address of the engineer to query
    /// * `offset` - Zero-based start index for pagination
    /// * `limit` - Maximum number of records to return (returns empty vec if 0)
    ///
    /// # Returns
    /// Vec containing the requested page of asset IDs
    ///
    /// # Panics
    /// - [`ContractError::IndexOutOfBounds`] if `offset` >= history length
    pub fn get_eng_history_page(env: Env, engineer: Address, offset: u32, limit: u32) -> Vec<u64> {
        let history: Vec<u64> = env
            .storage()
            .persistent()
            .get(&engineer_history_key(&engineer))
            .unwrap_or_else(|| Vec::new(&env));

        let len = history.len();
        if limit == 0 {
            return Vec::new(&env);
        }
        if offset >= len {
            return Vec::new(&env);
        }

        let end = (offset + limit).min(len);
        let mut page = Vec::new(&env);
        for i in offset..end {
            page.push_back(history.get(i).unwrap());
        }
        page
    }

    /// Admin-only function to update the asset registry address.
    /// Useful for registry migrations or updates.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored config admin
    /// * `new_registry` - The new asset registry contract address
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn update_asset_registry(env: Env, admin: Address, new_registry: Address) {
        ensure_not_paused(&env);
        admin.require_auth();

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        if is_zero_address(&env, &new_registry) {
            panic_with_error!(&env, ContractError::ZeroAddress);
        }
        if new_registry == get_engineer_registry_addr(&env) {
            panic_with_error!(&env, ContractError::SameRegistryAddress);
        }

        set_asset_registry_addr(&env, &new_registry);

        env.events()
            .publish((EVENT_REG_AST,), (admin.clone(), new_registry.clone()));
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("REG_AST")),
            (admin, env.ledger().timestamp(), new_registry),
        );
    }

    /// Get the address of the engineer registry contract.
    ///
    /// # Returns
    /// The address of the currently configured engineer registry
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    pub fn get_engineer_registry(env: Env) -> Address {
        get_engineer_registry_addr(&env)
    }

    /// Admin-only function to update the engineer registry address.
    /// Useful for registry migrations or updates.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored config admin
    /// * `new_registry` - The new engineer registry contract address
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn update_engineer_registry(env: Env, admin: Address, new_registry: Address) {
        ensure_not_paused(&env);
        admin.require_auth();

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        if is_zero_address(&env, &new_registry) {
            panic_with_error!(&env, ContractError::ZeroAddress);
        }
        if new_registry == get_asset_registry_addr(&env) {
            panic_with_error!(&env, ContractError::SameRegistryAddress);
        }

        set_engineer_registry_addr(&env, &new_registry);

        env.events()
            .publish((EVENT_REG_ENG,), (admin.clone(), new_registry.clone()));
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("REG_ENG")),
            (admin, env.ledger().timestamp(), new_registry),
        );
    }

    /// Get the current configuration of the lifecycle contract.
    ///
    /// # Returns
    /// The complete Config struct with all current settings
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    pub fn get_config(env: Env) -> Config {
        env.storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized))
    }

    /// Propose a WASM upgrade for the lifecycle contract.
    /// The upgrade must be executed after the timelock delay has passed.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored config admin
    /// * `new_wasm_hash` - The hash of the new WASM to deploy
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn propose_upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) {
        ensure_not_paused(&env);
        admin.require_auth();

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        store_timelock(&env, symbol_short!("UPGRADE"));
        env.storage()
            .persistent()
            .set(&symbol_short!("PEND_UPG"), &new_wasm_hash);
        env.storage()
            .persistent()
            .extend_ttl(&symbol_short!("PEND_UPG"), TTL_THRESHOLD, TTL_TARGET);

        env.events().publish(
            (symbol_short!("PROP_UPG"), admin.clone()),
            (new_wasm_hash, env.ledger().timestamp()),
        );
    }

    /// Execute a previously proposed WASM upgrade after the timelock delay has expired.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored config admin
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    /// - [`ContractError::ProposalNotFound`] if no upgrade was proposed or already executed
    /// - [`ContractError::TimelockNotExpired`] if the delay has not elapsed
    pub fn execute_upgrade(env: Env, admin: Address) {
        ensure_not_paused(&env);
        admin.require_auth();

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        require_timelock_ready(&env, symbol_short!("UPGRADE"));

        let new_wasm_hash: BytesN<32> = env
            .storage()
            .persistent()
            .get(&symbol_short!("PEND_UPG"))
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::ProposalNotFound));
        env.storage()
            .persistent()
            .remove(&symbol_short!("PEND_UPG"));

        env.events().publish(
            (symbol_short!("UPGRADE"), admin.clone()),
            new_wasm_hash.clone(),
        );
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("UPGRADE")),
            (admin, env.ledger().timestamp(), new_wasm_hash.clone()),
        );

        #[cfg(not(test))]
        {
            env.deployer().update_current_contract_wasm(new_wasm_hash);
        }
    }

    /// Admin-only: reset an asset's collateral score to zero.
    ///
    /// Use this after a major incident, asset rebuild, or verified fraud event
    /// to clear the score and force re-establishment of the maintenance record.
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if the contract has not been initialized.
    /// - [`ContractError::UnauthorizedAdmin`] if `admin` does not match the stored config admin.
    pub fn reset_score(env: Env, admin: Address, asset_id: u64) {
        ensure_not_paused(&env);
        admin.require_auth();

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        let now = env.ledger().timestamp();
        // Clear the maintenance history so compute_decay returns 0 after reset.
        let empty_history: Vec<MaintenanceRecord> = Vec::new(&env);
        env.storage().persistent().set(&history_key(asset_id), &empty_history);
        env.storage().persistent().extend_ttl(&history_key(asset_id), TTL_THRESHOLD, TTL_TARGET);
        env.storage().persistent().set(&score_key(asset_id), &0u32);
        env.storage()
            .persistent()
            .extend_ttl(&score_key(asset_id), TTL_THRESHOLD, TTL_TARGET);
        env.storage()
            .persistent()
            .set(&last_update_key(asset_id), &now);
        env.storage().persistent().extend_ttl(
            &last_update_key(asset_id),
            TTL_THRESHOLD,
            TTL_TARGET,
        );
        score_history_push(
            &env,
            asset_id,
            ScoreEntry {
                timestamp: now,
                score: 0,
            },
            config.max_history,
        );

        env.events()
            .publish((EVENT_RST_SCR, asset_id), (admin.clone(), now));
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("RST_SCR")),
            (admin, now, asset_id),
        );
    }

    /// Check collateral eligibility for multiple assets in a single call.
    ///
    /// # Arguments
    /// * `asset_ids` - Vec of asset IDs to check
    ///
    /// # Returns
    /// Vec of `bool` in the same order as `asset_ids`; each entry is `true` if
    /// the corresponding asset meets the eligibility threshold.
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::AssetNotFound`] if any asset ID does not exist
    pub fn batch_is_collateral_eligible(env: Env, asset_ids: Vec<u64>) -> Vec<bool> {
        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        let asset_registry = get_asset_registry_addr(&env);
        let asset_registry_client = asset_registry::AssetRegistryClient::new(&env, &asset_registry);
        let mut results: Vec<bool> = Vec::new(&env);
        for asset_id in asset_ids.iter() {
            // Verify asset exists (panics with AssetNotFound if not)
            asset_registry_client.get_asset(&asset_id);
            // Use read-only decay computation — no storage writes
            results.push_back(compute_decay(&env, asset_id) >= config.eligibility_threshold);
        }
        results
    }

    /// Admin-only function to prune a specific asset's history to the current max_history cap.
    ///
    /// Truncates both maintenance history and score history to not exceed the current
    /// `max_history` setting. Useful when `max_history` has been reduced and you need
    /// to immediately enforce the new cap on existing assets.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored config admin
    /// * `asset_id` - The unique identifier of the asset to prune
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn prune_asset_history(env: Env, admin: Address, asset_id: u64) {
        ensure_not_paused(&env);
        admin.require_auth();

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        // Prune maintenance history if it exceeds max_history
        let history_key = history_key(asset_id);
        if let Some(history) = env
            .storage()
            .persistent()
            .get::<_, Vec<MaintenanceRecord>>(&history_key)
        {
            if history.len() > config.max_history {
                // Keep only the last max_history entries
                let start_idx = history.len() - config.max_history;
                let mut pruned = Vec::new(&env);
                for i in start_idx..history.len() {
                    pruned.push_back(history.get(i).unwrap());
                }
                env.storage().persistent().set(&history_key, &pruned);
                env.storage()
                    .persistent()
                    .extend_ttl(&history_key, TTL_THRESHOLD, TTL_TARGET);

                // Remove asset from engineer index for engineers whose records were
                // entirely dropped (i.e. they appear only in the pruned prefix).
                let mut retained_engs: Vec<Address> = Vec::new(&env);
                for i in start_idx..history.len() {
                    let eng = history.get(i).unwrap().engineer;
                    let mut found = false;
                    for existing in retained_engs.iter() {
                        if existing == eng {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        retained_engs.push_back(eng);
                    }
                }
                let mut removed_engs: Vec<Address> = Vec::new(&env);
                for i in 0..start_idx {
                    let eng = history.get(i).unwrap().engineer;
                    let mut in_retained = false;
                    for existing in retained_engs.iter() {
                        if existing == eng {
                            in_retained = true;
                            break;
                        }
                    }
                    if in_retained {
                        continue;
                    }
                    let mut already_removed = false;
                    for existing in removed_engs.iter() {
                        if existing == eng {
                            already_removed = true;
                            break;
                        }
                    }
                    if !already_removed {
                        removed_engs.push_back(eng.clone());
                        engineer_history_remove(&env, &eng, asset_id);
                    }
                }
            }
        }

        // Prune score history if it exceeds max_history
        let score_history_key_val = score_history_key(asset_id);
        if let Some(score_history) = env
            .storage()
            .persistent()
            .get::<_, Vec<ScoreEntry>>(&score_history_key_val)
        {
            if score_history.len() > config.max_history {
                // Keep only the last max_history entries
                let start_idx = score_history.len() - config.max_history;
                let mut pruned = Vec::new(&env);
                for i in start_idx..score_history.len() {
                    pruned.push_back(score_history.get(i).unwrap());
                }
                env.storage()
                    .persistent()
                    .set(&score_history_key_val, &pruned);
                env.storage().persistent().extend_ttl(
                    &score_history_key_val,
                    TTL_THRESHOLD,
                    TTL_TARGET,
                );
            }
        }

        env.events()
            .publish((symbol_short!("PRUNE"), admin.clone()), asset_id);
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("PRUNE")),
            (admin, env.ledger().timestamp(), asset_id),
        );
    }

    /// Remove all lifecycle data for a deregistered asset.
    ///
    /// After `deregister_asset` is called on the asset registry the asset record is gone,
    /// but maintenance history, collateral score, score history, and the last-update
    /// timestamp remain in lifecycle storage. Call this function to reclaim that storage
    /// and prevent stale data from being read by anyone who knows the asset ID.
    ///
    /// This is a no-op for keys that do not exist (safe to call on already-clean assets).
    ///
    /// # Arguments
    /// * `admin` - The lifecycle admin address
    /// * `asset_id` - The unique identifier of the deregistered asset
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn purge_asset_data(env: Env, admin: Address, asset_id: u64) {
        ensure_not_paused(&env);
        admin.require_auth();

        let config: Config = env
            .storage()
            .persistent()
            .get(&CONFIG)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if config.admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        // Remove the asset ID from the history of all engineers who worked on it
        let history_key_val = history_key(asset_id);
        if let Some(history) = env
            .storage()
            .persistent()
            .get::<_, Vec<MaintenanceRecord>>(&history_key_val)
        {
            let mut engineers = Vec::new(&env);
            for record in history.iter() {
                let eng = record.engineer;
                // Check if already in our engineers list to avoid redundant removals
                let mut found = false;
                for existing in engineers.iter() {
                    if existing == eng {
                        found = true;
                        break;
                    }
                }
                if !found {
                    engineers.push_back(eng.clone());
                    engineer_history_remove(&env, &eng, asset_id);
                }
            }
        }

        env.storage().persistent().remove(&history_key(asset_id));
        env.storage().persistent().remove(&score_key(asset_id));
        env.storage()
            .persistent()
            .remove(&score_history_key(asset_id));
        env.storage()
            .persistent()
            .remove(&last_update_key(asset_id));

        env.events()
            .publish((symbol_short!("PURGE"), admin.clone()), asset_id);
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("PURGE")),
            (admin, env.ledger().timestamp(), asset_id),
        );
    }

    /// Request a loan against a collateral-eligible asset.
    ///
    /// # Arguments
    /// * `asset_id`  - The asset to use as collateral
    /// * `threshold` - Minimum vouch count required; must be > 0
    /// * `amount`    - Loan amount in stroops; must be > 0
    ///
    /// # Panics
    /// - [`ContractError::InvalidConfig`] if `threshold` or `amount` is 0
    /// - [`ContractError::NotInitialized`] if contract has not been initialized
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    pub fn request_loan(env: Env, asset_id: u64, threshold: u32, amount: i128) -> bool {
        if threshold == 0 {
            panic_with_error!(&env, ContractError::InvalidConfig);
        }
        if amount == 0 {
            panic_with_error!(&env, ContractError::InvalidConfig);
        }
        Self::is_collateral_eligible(env, asset_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::engineer_registry::{EngineerRegistry, EngineerRegistryClient};
    use asset_registry::{AssetRegistry, AssetRegistryClient};
    use soroban_sdk::{
        symbol_short,
        testutils::{storage::Persistent as _, Address as _, Events, Ledger},
        BytesN, Env, String, Symbol, TryIntoVal,
    };

    fn setup<'a>(
        env: &'a Env,
        max_history: u32,
    ) -> (
        LifecycleClient<'a>,
        AssetRegistryClient<'a>,
        EngineerRegistryClient<'a>,
        Address,
    ) {
        let asset_registry_id = env.register(AssetRegistry, ());
        let engineer_registry_id = env.register(EngineerRegistry, ());
        let lifecycle_id = env.register(Lifecycle, ());
        let admin = Address::generate(env);
        let asset_admin = Address::generate(env);

        let lifecycle = LifecycleClient::new(env, &lifecycle_id);
        lifecycle.initialize(
            &admin,
            &asset_registry_id,
            &engineer_registry_id,
            &admin,
            &max_history,
        );

        let asset_registry = AssetRegistryClient::new(env, &asset_registry_id);
        asset_registry.initialize_admin(&asset_admin, &asset_admin);
        asset_registry.add_asset_type(&asset_admin, &symbol_short!("GENSET"));

        (
            lifecycle,
            asset_registry,
            EngineerRegistryClient::new(env, &engineer_registry_id),
            admin,
        )
    }

    /// Generate a unique serial number string for each test asset registration.
    fn unique_serial(env: &Env) -> String {
        use core::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Build "SN-<n>" without std::format! (crate is no_std)
        let mut buf = [0u8; 24];
        buf[0] = b'S'; buf[1] = b'N'; buf[2] = b'-';
        let mut end = 24usize;
        let mut v = if n == 0 { 1u64 } else { n };
        while v > 0 {
            end -= 1;
            buf[end] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        let digit_len = 24 - end;
        let mut out = [0u8; 24];
        out[0] = b'S'; out[1] = b'N'; out[2] = b'-';
        out[3..3 + digit_len].copy_from_slice(&buf[end..24]);
        let s = core::str::from_utf8(&out[..3 + digit_len]).unwrap_or("SN-1");
        String::from_str(env, s)
    }

    fn register_asset(env: &Env, registry_client: &AssetRegistryClient) -> (u64, Address) {
        let owner = Address::generate(env);
        let serial = unique_serial(env);
        let asset_id = registry_client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(env, "Caterpillar 3516"),
            &serial,
            &owner,
        );
        (asset_id, owner)
    }

    fn register_asset_for_owner(
        env: &Env,
        registry_client: &AssetRegistryClient,
        owner: &Address,
    ) -> u64 {
        let serial = unique_serial(env);
        registry_client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(env, "Caterpillar 3516"),
            &serial,
            owner,
        )
    }

    fn register_engineer(env: &Env, registry_client: &EngineerRegistryClient) -> Address {
        let engineer = Address::generate(env);
        let issuer = Address::generate(env);
        let admin = Address::generate(env);
        let hash = BytesN::from_array(env, &[1u8; 32]);
        registry_client.initialize_admin(&admin, &admin);
        registry_client.add_trusted_issuer(&admin, &issuer);
        registry_client.register_engineer(&engineer, &hash, &issuer, &31_536_000);
        engineer
    }

    #[test]
    fn test_submit_and_score() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // 10 maintenance events at default score_increment (5) each = 50 points
        for _ in 0..10 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "Routine oil change"),
                &engineer,
            );
        }

        assert_eq!(client.get_collateral_score(&asset_id), 50);
        assert_eq!(client.get_maintenance_history(&asset_id).len(), 10);
    }

    #[test]
    fn test_collateral_score_monotonically_increases_with_verified_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let owner = Address::generate(&env);
        let asset_id = asset_registry_client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Caterpillar 3516"),
            &unique_serial(&env),
            &owner,
        );
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&owner, &asset_id, &engineer);

        let task_types = [
            symbol_short!("OIL_CHG"),
            symbol_short!("FILTER"),
            symbol_short!("ENGINE"),
            symbol_short!("INSPECT"),
        ];
        let notes = [
            "Oil change",
            "Filter replacement",
            "Engine overhaul",
            "Inspection follow-up",
        ];

        let mut previous_score = client.get_collateral_score(&asset_id);
        for (task_type, note) in task_types.iter().zip(notes.iter()) {
            client.submit_maintenance(
                &asset_id,
                task_type,
                &String::from_str(&env, note),
                &engineer,
            );
            env.ledger().with_mut(|li| li.timestamp += 1);

            let current_score = client.get_collateral_score(&asset_id);
            assert!(
                current_score >= previous_score,
                "Collateral score must not decrease after additional verified maintenance: {} -> {}",
                previous_score,
                current_score,
            );
            previous_score = current_score;
        }

        assert_eq!(client.get_maintenance_history(&asset_id).len(), 4);
    }

    #[test]
    fn test_authorized_engineer_can_submit_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let owner = Address::generate(&env);
        let asset_id = register_asset_for_owner(&env, &asset_registry_client, &owner);
        let engineer = register_engineer(&env, &engineer_registry_client);

        client.authorize_engineer(&owner, &asset_id, &engineer);
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "authorized"),
            &engineer,
        );

        assert_eq!(client.get_maintenance_history(&asset_id).len(), 1);
    }

    #[test]
    fn test_unauthorized_engineer_cannot_submit_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, _asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        // Intentionally NOT authorizing the engineer so the submission fails

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "unauthorized"),
            &engineer,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::EngineerNotAuthorized as u32,
            ))),
        );
    }

    #[test]
    fn test_non_owner_cannot_authorize_engineer() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let owner = Address::generate(&env);
        let rogue = Address::generate(&env);
        let asset_id = register_asset_for_owner(&env, &asset_registry_client, &owner);
        let engineer = register_engineer(&env, &engineer_registry_client);

        let result = client.try_authorize_engineer(&rogue, &asset_id, &engineer);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedOwner as u32,
            ))),
        );
    }

    #[test]
    fn test_get_maintenance_history_nonexistent_asset_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let result = client.try_get_maintenance_history(&999u64);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_submit_maintenance_nonexistent_asset() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, engineer_registry_client, _) = setup(&env, 0);
        let engineer = register_engineer(&env, &engineer_registry_client);

        let result = client.try_submit_maintenance(
            &999u64,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Should fail"),
            &engineer,
        );

        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_history_cap_enforced() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 3);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        for _ in 0..3 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "ok"),
                &engineer,
            );
        }

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "over cap"),
            &engineer,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::HistoryCapReached as u32,
            ))),
        );
    }

    #[test]
    fn test_history_cap_checked_before_cross_contract_calls() {
        // When the cap is already reached, HistoryCapReached must fire even if the
        // engineer is not registered — proving the check happens before cross-contract calls.
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 3);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        for _ in 0..3 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "ok"),
                &engineer,
            );
        }

        // Use an unregistered engineer — if cap check is first we get HistoryCapReached,
        // not UnauthorizedEngineer.
        let unregistered = Address::generate(&env);
        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "over cap"),
            &unregistered,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::HistoryCapReached as u32,
            ))),
        );
    }

    #[test]
    fn test_submit_maintenance_rejects_empty_task_type() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!(""),
            &String::from_str(&env, "Empty task type"),
            &engineer,
        );

        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidTaskType as u32,
            ))),
        );
    }

    #[test]
    fn test_submit_maintenance_rejects_unknown_task_type() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("UNKNOWN"),
            &String::from_str(&env, "Unknown task type"),
            &engineer,
        );

        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidTaskType as u32,
            ))),
        );
    }

    #[test]
    fn test_submit_maintenance_rejects_oversized_notes() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let oversized_notes = String::from_str(&env, &"x".repeat(300));

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &oversized_notes,
            &engineer,
        );

        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotesTooLong as u32,
            ))),
        );
    }

    #[test]
    fn test_unregistered_engineer_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let unregistered = Address::generate(&env);

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Should fail"),
            &unregistered,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedEngineer as u32,
            ))),
        );
    }

    #[test]
    fn test_maintenance_history_by_engineer() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id1, asset_owner1) = register_asset(&env, &asset_registry_client);
        let (asset_id2, asset_owner2) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner1, &asset_id1, &engineer);
        client.authorize_engineer(&asset_owner2, &asset_id2, &engineer);

        client.submit_maintenance(
            &asset_id1,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "one"),
            &engineer,
        );
        client.submit_maintenance(
            &asset_id2,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "two"),
            &engineer,
        );

        let history = client.get_engineer_maintenance_history(&engineer);
        assert_eq!(history.len(), 2);
        assert!(history.contains(&asset_id1));
        assert!(history.contains(&asset_id2));

        let other_engineer = Address::generate(&env);
        let empty_history = client.get_engineer_maintenance_history(&other_engineer);
        assert_eq!(empty_history.len(), 0);
    }
    #[test]
    fn test_engineer_history_bounded() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 3);
        let engineer = register_engineer(&env, &engineer_registry_client);

        // Register and maintain 5 different assets (exceeds max_history=3)
        let mut asset_ids = Vec::new(&env);
        for _ in 0..5 {
            let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
            asset_ids.push_back(asset_id);
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "service"),
                &engineer,
            );
        }

        // Engineer history should be capped at max_history (3)
        let history = client.get_engineer_maintenance_history(&engineer);
        assert_eq!(
            history.len(),
            3,
            "Engineer history should be bounded by max_history"
        );

        // Oldest entries (asset_ids[0], asset_ids[1]) should have been evicted
        assert!(!history.contains(&asset_ids.get(0).unwrap()));
        assert!(!history.contains(&asset_ids.get(1).unwrap()));

        // Newest entries should remain
        assert!(history.contains(&asset_ids.get(2).unwrap()));
        assert!(history.contains(&asset_ids.get(3).unwrap()));
        assert!(history.contains(&asset_ids.get(4).unwrap()));
    }

    #[test]
    fn test_engineer_history_no_duplicate_asset_id_on_repeated_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "first"),
            &engineer,
        );
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("INSPECT"),
            &String::from_str(&env, "second"),
            &engineer,
        );

        let history = client.get_engineer_maintenance_history(&engineer);
        assert_eq!(history.len(), 1);
        assert!(history.contains(&asset_id));
    }

    #[test]
    fn test_get_last_service_no_history() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        assert_eq!(client.get_last_service(&asset_id), None);
    }

    #[test]
    fn test_get_last_service_no_asset() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        assert_eq!(client.get_last_service(&9999u64), None);
    }

    #[test]
    fn test_get_last_service_returns_most_recent_by_timestamp() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Submit first record at t=1000
        env.ledger().set_timestamp(1000);
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "first"),
            &engineer,
        );

        // Submit second record at t=2000 (most recent)
        env.ledger().set_timestamp(2000);
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("INSPECT"),
            &String::from_str(&env, "second"),
            &engineer,
        );

        let last = client.get_last_service(&asset_id).unwrap();
        assert_eq!(last.timestamp, 2000);
        assert_eq!(last.task_type, symbol_short!("INSPECT"));
    }

    #[test]
    fn test_admin_can_update_score_increment() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.update_score_increment(&admin, &12);
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Configured increment"),
            &engineer,
        );

        // score_increment (12) governs scoring, not task weight
        assert_eq!(client.get_collateral_score(&asset_id), 12);
    }

    #[test]
    fn test_score_increment_affects_scoring() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Default score_increment is 5
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "First"),
            &engineer,
        );
        assert_eq!(client.get_collateral_score(&asset_id), 5);

        // Update score_increment to 8
        client.update_score_increment(&admin, &8);
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("FILTER"),
            &String::from_str(&env, "Second"),
            &engineer,
        );
        // Score should be 5 + 8 = 13
        assert_eq!(client.get_collateral_score(&asset_id), 13);

        // Update score_increment to 20 (capped at 100)
        client.update_score_increment(&admin, &20);
        for _ in 0..5 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("ENGINE"),
                &String::from_str(&env, "Bulk"),
                &engineer,
            );
        }
        // 13 + 5*20 = 113, capped at 100
        assert_eq!(client.get_collateral_score(&asset_id), 100);
    }

    #[test]
    fn test_non_admin_cannot_update_score_increment() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let outsider = Address::generate(&env);
        let result = client.try_update_score_increment(&outsider, &12);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_update_score_increment_zero_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let result = client.try_update_score_increment(&admin, &0);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidConfig as u32,
            ))),
        );
    }

    #[test]
    fn test_update_score_increment_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let old_increment = client.get_config().score_increment;
        let new_increment: u32 = 12;

        client.update_score_increment(&admin, &new_increment);

        let events = env.events().all();
        assert!(events.len() >= 1);
        let (_, topics, data) = events.get(0).unwrap();
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("CFG_UPD"));

        let (emitted_old, emitted_new): (u32, u32) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_old, old_increment);
        assert_eq!(emitted_new, new_increment);
    }

    #[test]
    fn test_admin_can_update_max_history() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        client.update_max_history(&admin, &300);
        let config = client.get_config();
        assert_eq!(config.max_history, 300);
    }

    #[test]
    fn test_non_admin_cannot_update_max_history() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let outsider = Address::generate(&env);
        let result = client.try_update_max_history(&outsider, &300);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_update_max_history_zero_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let result = client.try_update_max_history(&admin, &0);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidConfig as u32,
            ))),
        );
    }

    #[test]
    fn test_score_history_bounded_after_max_history_update() {
        let env = Env::default();
        env.mock_all_auths();

        // Setup with initial max_history of 10
        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 10);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Submit 4 maintenance records (below max_history of 10)
        for _i in 0..4 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "Maintenance"),
                &engineer,
            );
            env.ledger().set_timestamp(env.ledger().timestamp() + 1000);
        }

        // Verify score history has 4 entries
        let history = client.get_score_history(&asset_id);
        assert_eq!(history.len(), 4u32);

        // Update max_history to 5 - from now on, history should be capped at 5
        client.update_max_history(&admin, &5);

        // Submit one more maintenance record up to the new cap
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Maintenance"),
            &engineer,
        );
        env.ledger().set_timestamp(env.ledger().timestamp() + 1000);

        // Call decay_score which will use the new max_history value
        client.decay_score(&asset_id);

        // Verify score history is now bounded to the new max_history (5)
        let history_after = client.get_score_history(&asset_id);
        assert!(
            history_after.len() <= 5u32,
            "Score history {} should be <= 5 after max_history update",
            history_after.len()
        );
    }

    #[test]
    fn test_max_history_reduction_does_not_automatically_prune() {
        let env = Env::default();
        env.mock_all_auths();

        // Setup with initial max_history of 10
        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 10);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Submit 10 maintenance records to reach max_history
        for _i in 0..10 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "Maintenance"),
                &engineer,
            );
        }

        // Verify both histories have 10 entries
        let history = client.get_maintenance_history(&asset_id);
        let score_history = client.get_score_history(&asset_id);
        assert_eq!(history.len(), 10u32);
        assert_eq!(score_history.len(), 10u32);

        // Reduce max_history to 3
        client.update_max_history(&admin, &3);

        // Verify that existing histories were NOT pruned automatically
        let history_after = client.get_maintenance_history(&asset_id);
        let score_history_after = client.get_score_history(&asset_id);
        assert_eq!(
            history_after.len(),
            10u32,
            "Maintenance history should remain at 10 until next write"
        );
        assert_eq!(
            score_history_after.len(),
            10u32,
            "Score history should remain at 10 until next write"
        );
    }

    #[test]
    fn test_prune_asset_history_reduces_both_histories() {
        let env = Env::default();
        env.mock_all_auths();

        // Setup with initial max_history of 10
        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 10);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Submit 10 maintenance records to reach max_history
        for _i in 0..10 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "Maintenance"),
                &engineer,
            );
        }

        // Reduce max_history and verify histories still at 10
        client.update_max_history(&admin, &3);
        let history_before = client.get_maintenance_history(&asset_id);
        let score_history_before = client.get_score_history(&asset_id);
        assert_eq!(history_before.len(), 10u32);
        assert_eq!(score_history_before.len(), 10u32);

        // Call prune_asset_history to immediately prune to the new cap
        client.prune_asset_history(&admin, &asset_id);

        // Verify both histories are now pruned to max_history of 3
        let history_after = client.get_maintenance_history(&asset_id);
        let score_history_after = client.get_score_history(&asset_id);
        assert_eq!(
            history_after.len(),
            3u32,
            "Maintenance history should be pruned to 3"
        );
        assert_eq!(
            score_history_after.len(),
            3u32,
            "Score history should be pruned to 3"
        );

        // Verify that the most recent entries were kept (not the oldest)
        let last_before = history_before.get(9).unwrap();
        let last_after = history_after.get(2).unwrap();
        assert_eq!(
            last_before.timestamp, last_after.timestamp,
            "Most recent entries should be kept"
        );
    }

    #[test]
    fn test_prune_asset_history_cleans_engineer_index() {
        let env = Env::default();
        env.mock_all_auths();

        // max_history starts at 10
        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 10);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);

        // Two engineers: eng_a submits the first 5 records, eng_b submits the next 5
        let eng_a = register_engineer(&env, &engineer_registry_client);
        let eng_b = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &eng_a);
        client.authorize_engineer(&asset_owner, &asset_id, &eng_b);

        for _ in 0..5 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("INSPECT"),
                &String::from_str(&env, "Early check"),
                &eng_a,
            );
        }
        for _ in 0..5 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "Later check"),
                &eng_b,
            );
        }

        // Both engineers should be in the index before pruning
        assert!(client
            .get_engineer_maintenance_history(&eng_a)
            .contains(&asset_id));
        assert!(client
            .get_engineer_maintenance_history(&eng_b)
            .contains(&asset_id));

        // Reduce max_history to 5 so eng_a's records are entirely pruned
        client.update_max_history(&admin, &5);
        client.prune_asset_history(&admin, &asset_id);

        // eng_a no longer has any retained records → must be removed from index
        assert!(
            !client
                .get_engineer_maintenance_history(&eng_a)
                .contains(&asset_id),
            "eng_a should be removed from engineer index after all their records are pruned"
        );
        // eng_b still has retained records → must remain in index
        assert!(
            client
                .get_engineer_maintenance_history(&eng_b)
                .contains(&asset_id),
            "eng_b should remain in engineer index because their records were kept"
        );
    }

    #[test]
    fn test_non_admin_cannot_prune_asset_history() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 10);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let outsider = Address::generate(&env);

        let result = client.try_prune_asset_history(&outsider, &asset_id);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_admin_can_update_decay_config() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build up a score first (default score_increment = 5)
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Major overhaul"),
            &engineer,
        );

        let initial_score = client.get_collateral_score(&asset_id);

        // Update decay config: 2 points per 60 seconds (for testing)
        client.update_decay_config(&admin, &2, &60);

        // Advance ledger time by 120 seconds (2 intervals)
        env.ledger()
            .with_mut(|li| li.timestamp = li.timestamp + 120);

        // Apply decay: should lose 4 points (2 * 2 intervals)
        client.decay_score(&asset_id);
        let new_score = client.get_collateral_score(&asset_id);

        assert_eq!(new_score, initial_score.saturating_sub(4));
    }

    #[test]
    fn test_update_decay_config_persists_via_get_config() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        client.update_decay_config(&admin, &7, &3600);

        let config = client.get_config();
        assert_eq!(config.decay_rate, 7);
        assert_eq!(config.decay_interval, 3600);
    }

    #[test]
    fn test_non_admin_cannot_update_decay_config() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let outsider = Address::generate(&env);
        let result = client.try_update_decay_config(&outsider, &10, &60);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_update_decay_config_zero_interval_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let result = client.try_update_decay_config(&admin, &10, &0);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidConfig as u32,
            ))),
        );
    }

    #[test]
    fn test_update_decay_config_zero_rate_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let result = client.try_update_decay_config(&admin, &0, &2592000);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidConfig as u32,
            ))),
        );
    }

    #[test]
    fn test_decay_score_uses_configured_values() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build up a score to 25 (5 * default score_increment of 5)
        for _ in 0..5 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("ENGINE"),
                &String::from_str(&env, "Major work"),
                &engineer,
            );
        }

        let initial_score = client.get_collateral_score(&asset_id);

        // Set custom decay: 2 points per 100 seconds
        client.update_decay_config(&admin, &2, &100);

        // Advance time by 250 seconds (2 full intervals)
        env.ledger()
            .with_mut(|li| li.timestamp = li.timestamp + 250);

        // Apply decay: should lose 4 points (2 * 2 intervals)
        client.decay_score(&asset_id);
        let new_score = client.get_collateral_score(&asset_id);

        assert_eq!(new_score, initial_score.saturating_sub(4));
    }

    #[test]
    fn test_get_collateral_score_applies_lazy_decay() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build score to 20 (default score_increment = 5)
        for _ in 0..4 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("ENGINE"),
                &String::from_str(&env, "Build score"),
                &engineer,
            );
        }

        // Fast decay: 5 points per 60 seconds
        client.update_decay_config(&admin, &5, &60);

        // Advance 120 seconds (2 intervals -> 10 points decay)
        env.ledger()
            .with_mut(|li| li.timestamp = li.timestamp + 120);

        let decayed = client.get_collateral_score(&asset_id);
        assert_eq!(decayed, 10);

        // Ensure value is written back to storage (subsequent reads are consistent)
        let decayed_again = client.get_collateral_score(&asset_id);
        assert_eq!(decayed_again, 10);
    }

    #[test]
    fn test_get_collateral_score_is_read_only() {
        // get_collateral_score must NOT write the decayed score back to storage.
        // Calling it multiple times across ledger advances must always return the
        // score computed from the *original* stored value, not a previously-decayed one.
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build score to 20 (4 × default score_increment of 5)
        for _ in 0..4 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("ENGINE"),
                &String::from_str(&env, "Build score"),
                &engineer,
            );
        }
        assert_eq!(client.get_collateral_score(&asset_id), 20);

        // Fast decay: 5 points per 60 seconds
        client.update_decay_config(&admin, &5, &60);

        // Advance 60 s (1 interval → −5 pts → expected 15)
        env.ledger().with_mut(|li| li.timestamp += 60);
        assert_eq!(client.get_collateral_score(&asset_id), 15);

        // Advance another 60 s without calling decay_score.
        // If get_collateral_score had written 15 back to storage, the next call
        // would compute from 15 and return 10. But because it is read-only it must
        // still compute from the original stored value of 20 and return 10 (2 intervals).
        env.ledger().with_mut(|li| li.timestamp += 60);
        assert_eq!(client.get_collateral_score(&asset_id), 10);

        // Confirm the stored score is still 20 (untouched by get_collateral_score)
        let contract_id = client.address.clone();
        env.as_contract(&contract_id, || {
            let stored: u32 = env
                .storage()
                .persistent()
                .get(&score_key(asset_id))
                .unwrap_or(0);
            assert_eq!(
                stored, 20,
                "stored score must not be mutated by get_collateral_score"
            );
        });
    }

    #[test]
    fn test_decay_score_five_points_per_thirty_day_interval() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        for _ in 0..10 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("ENGINE"),
                &String::from_str(&env, "Build score to 50"),
                &engineer,
            );
        }
        assert_eq!(client.get_collateral_score(&asset_id), 50);

        env.ledger()
            .with_mut(|li| li.timestamp = li.timestamp + 2 * DEFAULT_DECAY_INTERVAL);

        let decayed = client.decay_score(&asset_id);
        assert_eq!(decayed, 40);
        assert_eq!(client.get_collateral_score(&asset_id), 40);
    }

    #[test]
    fn test_decay_score_clamps_at_zero_after_long_elapsed_time() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Single major service"),
            &engineer,
        );
        assert_eq!(client.get_collateral_score(&asset_id), 5);

        const SECONDS_PER_DAY: u64 = 86_400;
        const DAYS_PER_YEAR: u64 = 365;
        env.ledger().with_mut(|li| {
            li.timestamp = li.timestamp + DAYS_PER_YEAR * SECONDS_PER_DAY;
        });

        let decayed = client.decay_score(&asset_id);
        assert_eq!(decayed, 0);
        assert_eq!(client.get_collateral_score(&asset_id), 0);
    }

    #[test]
    fn test_decay_score_returns_zero_for_asset_with_no_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 0);
        let owner = Address::generate(&env);
        let asset_id = asset_registry_client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "No-maintenance asset"),
            &unique_serial(&env),
            &owner,
        );

        // Advance ledger so last_update_key unwrap_or(0) would produce a large time_elapsed
        env.ledger().with_mut(|li| li.timestamp += 10_000_000);

        // Score is 0 (never maintained) ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â early return must fire and return 0
        assert_eq!(client.decay_score(&asset_id), 0);
    }

    #[test]
    fn test_decay_score_returns_zero_for_nonexistent_asset_id() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);

        // Asset ID 9999 was never registered; score_key is absent ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ unwrap_or(0) ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ early return
        assert_eq!(client.decay_score(&9999u64), 0);
    }

    #[test]
    fn test_apply_decay_extends_last_update_ttl_when_score_is_zero() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build a score then reset it to 0 so last_update_key exists but score is 0
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "ok"),
            &engineer,
        );
        client.reset_score(&admin, &asset_id);
        assert_eq!(client.get_collateral_score(&asset_id), 0);

        // decay_score on a zero-score asset should return 0 and extend last_update_key TTL
        assert_eq!(client.decay_score(&asset_id), 0);

        let contract_id = client.address.clone();
        let ttl = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get_ttl(&last_update_key(asset_id))
        });
        assert!(
            ttl > 0,
            "last_update_key TTL should be extended even when score is 0"
        );
    }

    #[test]
    fn test_apply_decay_extends_ttl_on_zero_interval_early_return() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build a non-zero score
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "ok"),
            &engineer,
        );
        let initial_score = client.get_collateral_score(&asset_id);
        assert!(initial_score > 0);

        let contract_id = client.address.clone();

        // Verify keys exist before decay_score
        env.as_contract(&contract_id, || {
            assert!(env.storage().persistent().has(&score_key(asset_id)));
            assert!(env.storage().persistent().has(&last_update_key(asset_id)));
        });

        // Call decay_score with no time elapsed (zero intervals) -> early return path
        let score_after = client.decay_score(&asset_id);
        assert_eq!(score_after, initial_score);

        // Verify TTLs were extended even on the early-return path
        env.as_contract(&contract_id, || {
            assert!(
                env.storage().persistent().get_ttl(&score_key(asset_id)) > 0,
                "score_key TTL should be extended on zero-interval early return"
            );
            assert!(
                env.storage()
                    .persistent()
                    .get_ttl(&last_update_key(asset_id))
                    > 0,
                "last_update_key TTL should be extended on zero-interval early return"
            );
        });
    }

    #[test]
    fn test_submit_maintenance_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let task_type = symbol_short!("OIL_CHG");
        let timestamp = env.ledger().timestamp();
        client.submit_maintenance(
            &asset_id,
            &task_type,
            &String::from_str(&env, "Routine"),
            &engineer,
        );

        use soroban_sdk::{FromVal, TryIntoVal};
        let maint_topic = symbol_short!("maint");
        let events = env.events().all();
        let (_, topics, data) = events
            .iter()
            .find(|(_, topics, _)| {
                topics.get(0).map_or(false, |v| {
                    Symbol::from_val(&env, &v) == maint_topic
                })
            })
            .expect("maint event not emitted");

        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, maint_topic);

        let (emitted_asset_id, emitted_engineer, emitted_task_type, emitted_timestamp): (
            u64,
            Address,
            Symbol,
            u64,
        ) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_asset_id, asset_id);
        assert_eq!(emitted_engineer, engineer);
        assert_eq!(emitted_task_type, task_type);
        assert_eq!(emitted_timestamp, timestamp);
    }

    #[test]
    fn test_initialize_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let asset_registry_id = env.register(AssetRegistry, ());
        let engineer_registry_id = env.register(EngineerRegistry, ());
        let lifecycle_id = env.register(Lifecycle, ());
        let admin = Address::generate(&env);

        let lifecycle = LifecycleClient::new(&env, &lifecycle_id);
        lifecycle.initialize(
            &admin,
            &asset_registry_id,
            &engineer_registry_id,
            &admin,
            &0u32,
        );

        let events = env.events().all();
        assert!(events.len() >= 1);
    }

    #[test]
    fn test_initialize_twice_panics_with_already_initialized() {
        let env = Env::default();
        env.mock_all_auths();

        let asset_registry_id = env.register(AssetRegistry, ());
        let engineer_registry_id = env.register(EngineerRegistry, ());
        let lifecycle_id = env.register(Lifecycle, ());
        let admin = Address::generate(&env);

        let lifecycle = LifecycleClient::new(&env, &lifecycle_id);
        lifecycle.initialize(
            &admin,
            &asset_registry_id,
            &engineer_registry_id,
            &admin,
            &0u32,
        );

        // Try to initialize again
        let result = lifecycle.try_initialize(
            &admin,
            &asset_registry_id,
            &engineer_registry_id,
            &admin,
            &0u32,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AlreadyInitialized as u32,
            ))),
        );
    }

    #[test]
    fn test_initialize_rejects_same_registry_addresses() {
        let env = Env::default();
        env.mock_all_auths();

        let same_registry_id = env.register(AssetRegistry, ());
        let lifecycle_id = env.register(Lifecycle, ());
        let admin = Address::generate(&env);

        let lifecycle = LifecycleClient::new(&env, &lifecycle_id);
        let result =
            lifecycle.try_initialize(&admin, &same_registry_id, &same_registry_id, &admin, &0u32);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidConfig as u32,
            ))),
        );
    }

    #[test]
    fn test_get_collateral_score_unregistered_asset() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);

        // Query score for non-existent asset ID
        let result = client.try_get_collateral_score(&999u64);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_is_collateral_eligible_unregistered_asset() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);

        // Check eligibility for non-existent asset ID
        let result = client.try_is_collateral_eligible(&999u64);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_is_collateral_eligible_below_default_threshold() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // One maintenance record gives a low score (well below default threshold of 50)
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "notes"),
            &engineer,
        );

        assert!(!client.is_collateral_eligible(&asset_id));
    }

    #[test]
    fn test_is_collateral_eligible_after_threshold_lowered() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "notes"),
            &engineer,
        );

        // Score is low; lower threshold so asset becomes eligible
        let score = client.get_collateral_score(&asset_id);
        client.update_eligibility_threshold(&admin, &score);

        assert!(client.is_collateral_eligible(&asset_id));
    }

    #[test]
    fn test_is_collateral_eligible_flips_false_after_decay() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build score to exactly the eligibility threshold (50) via 10 ÃƒÆ’Ã¢â‚¬â€ FILTER (5 pts each)
        for _ in 0..10 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("FILTER"),
                &String::from_str(&env, "notes"),
                &engineer,
            );
        }
        assert!(client.is_collateral_eligible(&asset_id));

        // Fast decay: 5 points per 60 seconds; advance 2 intervals ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ -10 pts ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ score 40 < 50
        client.update_decay_config(&admin, &5, &60);
        env.ledger()
            .with_mut(|li| li.timestamp = li.timestamp + 120);

        assert!(!client.is_collateral_eligible(&asset_id));
    }

    #[test]
    fn test_full_cross_contract_threshold_boundary() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Set eligibility threshold to a deterministic value for boundary testing.
        client.update_eligibility_threshold(&admin, &10);

        // Just below threshold: one maintenance event (FILTER = 5 points)
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("FILTER"),
            &String::from_str(&env, "Filter replacement 1"),
            &engineer,
        );
        assert_eq!(client.get_collateral_score(&asset_id), 5);
        assert!(!client.is_collateral_eligible(&asset_id));

        // Cross threshold with one more event (total = 10)
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("FILTER"),
            &String::from_str(&env, "Filter replacement 2"),
            &engineer,
        );
        assert_eq!(client.get_collateral_score(&asset_id), 10);
        assert!(client.is_collateral_eligible(&asset_id));
    }

    #[test]
    fn test_update_eligibility_threshold_non_admin_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let outsider = Address::generate(&env);

        let result = client.try_update_eligibility_threshold(&outsider, &10);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_update_eligibility_threshold_zero_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);

        let result = client.try_update_eligibility_threshold(&admin, &0);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidConfig as u32,
            ))),
        );
    }

    #[test]
    fn test_batch_is_collateral_eligible_mixed() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let engineer = register_engineer(&env, &engineer_registry_client);

        // asset_a: 10 ÃƒÆ’Ã¢â‚¬â€ ENGINE (5 pts each) = 50 ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ eligible
        let (asset_a, asset_owner_a) = register_asset(&env, &asset_registry_client);
        for _ in 0..10 {
            client.submit_maintenance(
                &asset_a,
                &symbol_short!("ENGINE"),
                &String::from_str(&env, "ok"),
                &engineer,
            );
        }

        // asset_b: 1 ÃƒÆ’Ã¢â‚¬â€ OIL_CHG (5 pts) ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ not eligible
        let (asset_b, asset_owner_b) = register_asset(&env, &asset_registry_client);
        client.submit_maintenance(
            &asset_b,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "ok"),
            &engineer,
        );

        let mut ids = Vec::new(&env);
        ids.push_back(asset_a);
        ids.push_back(asset_b);

        let results = client.batch_is_collateral_eligible(&ids);
        assert_eq!(results.len(), 2);
        assert!(results.get(0).unwrap());
        assert!(!results.get(1).unwrap());
    }

    #[test]
    fn test_batch_is_collateral_eligible_empty_input() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let results = client.batch_is_collateral_eligible(&Vec::new(&env));
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_batch_is_collateral_eligible_unknown_asset_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let mut ids = Vec::new(&env);
        ids.push_back(999u64);

        let result = client.try_batch_is_collateral_eligible(&ids);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                asset_registry::ContractError::AssetNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_batch_is_collateral_eligible_no_state_mutation() {
        // batch_is_collateral_eligible must not write decayed scores back to storage.
        // The stored score before and after the batch call must be identical.
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let engineer = register_engineer(&env, &engineer_registry_client);

        let (asset_a, asset_owner_a) = register_asset(&env, &asset_registry_client);
        let (asset_b, asset_owner_b) = register_asset(&env, &asset_registry_client);

        // Give both assets a score above the default threshold (50)
        for _ in 0..10 {
            client.submit_maintenance(
                &asset_a,
                &symbol_short!("ENGINE"),
                &String::from_str(&env, "ok"),
                &engineer,
            );
            client.submit_maintenance(
                &asset_b,
                &symbol_short!("ENGINE"),
                &String::from_str(&env, "ok"),
                &engineer,
            );
        }

        // Capture the raw stored scores before time advance
        let contract_id = client.address.clone();
        let stored_a_before: u32 = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get(&score_key(asset_a))
                .unwrap_or(0)
        });
        let stored_b_before: u32 = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get(&score_key(asset_b))
                .unwrap_or(0)
        });

        // Advance time so decay would apply if storage were written
        env.ledger().with_mut(|l| l.timestamp += 2_592_000 * 3); // 3 decay intervals

        let mut ids = Vec::new(&env);
        ids.push_back(asset_a);
        ids.push_back(asset_b);
        client.batch_is_collateral_eligible(&ids);

        // Raw stored scores must be unchanged — batch call must not write to storage
        let stored_a_after: u32 = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get(&score_key(asset_a))
                .unwrap_or(0)
        });
        let stored_b_after: u32 = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get(&score_key(asset_b))
                .unwrap_or(0)
        });
        assert_eq!(
            stored_a_after, stored_a_before,
            "batch_is_collateral_eligible must not write decayed score to storage"
        );
        assert_eq!(
            stored_b_after, stored_b_before,
            "batch_is_collateral_eligible must not write decayed score to storage"
        );
    }

    // --- get_collateral_score_batch tests ---

    #[test]
    fn test_get_collateral_score_batch_single_element() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let engineer = register_engineer(&env, &engineer_registry_client);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "ok"),
            &engineer,
        );

        let mut ids = Vec::new(&env);
        ids.push_back(asset_id);
        let results = client.get_collateral_score_batch(&ids);
        assert_eq!(results.len(), 1);
        let (ret_id, ret_score) = results.get(0).unwrap();
        assert_eq!(ret_id, asset_id);
        assert_eq!(ret_score, client.get_collateral_score(&asset_id));
    }

    #[test]
    fn test_get_collateral_score_batch_multi_element() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let engineer = register_engineer(&env, &engineer_registry_client);

        let (asset_a, asset_owner_a) = register_asset(&env, &asset_registry_client);
        let (asset_b, asset_owner_b) = register_asset(&env, &asset_registry_client);
        client.authorize_engineer(&asset_owner_a, &asset_a, &engineer);
        client.authorize_engineer(&asset_owner_b, &asset_b, &engineer);

        for _ in 0..10 {
            client.submit_maintenance(
                &asset_a,
                &symbol_short!("ENGINE"),
                &String::from_str(&env, "ok"),
                &engineer,
            );
        }
        client.submit_maintenance(
            &asset_b,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "ok"),
            &engineer,
        );

        let mut ids = Vec::new(&env);
        ids.push_back(asset_a);
        ids.push_back(asset_b);
        let results = client.get_collateral_score_batch(&ids);
        assert_eq!(results.len(), 2);
        let (id_a, score_a) = results.get(0).unwrap();
        assert_eq!(id_a, asset_a);
        assert_eq!(score_a, client.get_collateral_score(&asset_a));
        let (id_b, score_b) = results.get(1).unwrap();
        assert_eq!(id_b, asset_b);
        assert_eq!(score_b, client.get_collateral_score(&asset_b));
    }

    #[test]
    fn test_get_collateral_score_batch_unknown_asset_is_skipped() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let engineer = register_engineer(&env, &engineer_registry_client);
        let known_id = register_asset(&env, &asset_registry_client);
        client.submit_maintenance(
            &known_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, ""),
            &engineer,
        );

        let mut ids = Vec::new(&env);
        ids.push_back(known_id);
        ids.push_back(999u64); // unknown

        // Unknown asset is silently skipped; only the known asset appears in results.
        let results = client.get_collateral_score_batch(&ids);
        assert_eq!(results.len(), 1);
        let (ret_id, ret_score) = results.get(0).unwrap();
        assert_eq!(ret_id, known_id);
        assert_eq!(ret_score, client.get_collateral_score(&known_id));
    }

    // --- Upgrade tests ---

    #[test]
    fn test_admin_can_propose_upgrade() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let new_wasm_hash = BytesN::from_array(&env, &[0xabu8; 32]);

        // propose_upgrade should succeed for admin
        let result = client.try_propose_upgrade(&admin, &new_wasm_hash);
        assert!(
            result
                != Err(Ok(soroban_sdk::Error::from_contract_error(
                    ContractError::UnauthorizedAdmin as u32
                )))
        );
    }

    #[test]
    fn test_non_admin_cannot_propose_upgrade() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let outsider = Address::generate(&env);
        let new_wasm_hash = BytesN::from_array(&env, &[0xabu8; 32]);

        let result = client.try_propose_upgrade(&outsider, &new_wasm_hash);
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

        let (client, _, _, admin) = setup(&env, 0);
        let new_wasm_hash = BytesN::from_array(&env, &[0xabu8; 32]);

        client.propose_upgrade(&admin, &new_wasm_hash);

        // Advance past timelock delay (48 hours)
        let base = env.ledger().timestamp();
        env.ledger().set_timestamp(base + TIMELOCK_DELAY_SECS + 1);

        client.execute_upgrade(&admin);

        let events = env.events().all();
        use soroban_sdk::TryIntoVal;
        let upgrade_event = events.iter().find(|(_, topics, _)| {
            if let Some(val) = topics.get(0) {
                if let Ok(s) = val.try_into_val::<_, Symbol>(&env) {
                    return s == symbol_short!("UPGRADE");
                }
            }
            false
        });
        assert!(upgrade_event.is_some(), "UPGRADE event must be emitted");
        let (_, _, data) = upgrade_event.unwrap();
        let emitted_hash: BytesN<32> = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_hash, new_wasm_hash);
    }

    // --- Score history tests ---

    #[test]
    fn test_propose_and_accept_admin_transfer() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let new_admin = Address::generate(&env);

        client.propose_admin(&admin, &new_admin);
        client.accept_admin();

        assert_eq!(client.get_config().admin, new_admin);
    }

    #[test]
    fn test_pending_admin_key_cleared_after_accept() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let new_admin = Address::generate(&env);

        client.propose_admin(&admin, &new_admin);
        client.accept_admin();

        let contract_id = client.address.clone();
        env.as_contract(&contract_id, || {
            assert!(!env.storage().instance().has(&PENDING_ADMIN_KEY));
        });
    }

    #[test]
    fn test_non_admin_cannot_propose_admin() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
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

        let (client, _, _, admin) = setup(&env, 0);
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
        assert_eq!(client.get_config().admin, admin);
    }

    // --- Score history tests (original) ---

    #[test]
    fn test_score_history_empty_before_any_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);

        let history = client.get_score_history(&asset_id);
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn test_score_history_records_entry_per_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "First"),
            &engineer,
        );
        env.ledger().with_mut(|li| li.timestamp += 1);
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Second"),
            &engineer,
        );
        env.ledger().with_mut(|li| li.timestamp += 1);
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("FILTER"),
            &String::from_str(&env, "Third"),
            &engineer,
        );

        let history = client.get_score_history(&asset_id);
        // One entry per maintenance event (each at a distinct timestamp)
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn test_score_history_scores_are_cumulative() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // All tasks use score_increment (default 5); advance ledger between each to ensure
        // distinct timestamps so deduplication does not collapse the entries.
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "a"),
            &engineer,
        );
        env.ledger().with_mut(|li| li.timestamp += 1);
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "b"),
            &engineer,
        );
        env.ledger().with_mut(|li| li.timestamp += 1);
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("FILTER"),
            &String::from_str(&env, "c"),
            &engineer,
        );

        let history = client.get_score_history(&asset_id);
        assert_eq!(history.get(0).unwrap().score, 5); // 0 + 5
        assert_eq!(history.get(1).unwrap().score, 10); // 5 + 5
        assert_eq!(history.get(2).unwrap().score, 15); // 10 + 5
    }

    #[test]
    fn test_score_history_timestamps_match_ledger() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let t0 = env.ledger().timestamp();
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "at t0"),
            &engineer,
        );

        env.ledger()
            .with_mut(|li| li.timestamp = li.timestamp + 1000);
        let t1 = env.ledger().timestamp();
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("LUBE"),
            &String::from_str(&env, "at t1"),
            &engineer,
        );

        let history = client.get_score_history(&asset_id);
        assert_eq!(history.get(0).unwrap().timestamp, t0);
        assert_eq!(history.get(1).unwrap().timestamp, t1);
    }

    #[test]
    fn test_score_history_capped_at_100() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // 20 tasks at default score_increment (5) each would be 100, then more should stay at 100.
        // Advance ledger by 1 second between each so every submission gets a distinct timestamp
        // and deduplication does not collapse entries into one.
        for _ in 0..22 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("REBUILD"),
                &String::from_str(&env, "major"),
                &engineer,
            );
            env.ledger().with_mut(|li| li.timestamp += 1);
        }

        let history = client.get_score_history(&asset_id);
        // Score should never exceed 100
        for i in 0..history.len() {
            assert!(history.get(i).unwrap().score <= 100);
        }
        // After 20 tasks the score is already 100; subsequent entries stay at 100
        assert_eq!(history.get(20).unwrap().score, 100);
        assert_eq!(history.get(21).unwrap().score, 100);
    }

    #[test]
    fn test_score_history_pruned_at_max_history() {
        let env = Env::default();
        env.mock_all_auths();

        // max_history = 5
        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 5);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Submit 5 records -- history_key is capped at 5, score_history must also stay at 5.
        // Advance ledger by 1 second between each so every submission gets a distinct timestamp.
        for _ in 0..5 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "ok"),
                &engineer,
            );
            env.ledger().with_mut(|li| li.timestamp += 1);
        }
        assert_eq!(client.get_score_history(&asset_id).len(), 5);

        // history_key is now full; further submit_maintenance calls are rejected,
        // so trigger score_history growth via decay_score instead.
        // Advance past one decay interval and call decay_score 3 more times.
        for _ in 0..3 {
            env.ledger()
                .with_mut(|li| li.timestamp += DEFAULT_DECAY_INTERVAL);
            client.decay_score(&asset_id);
        }

        // score_history must never exceed max_history
        assert_eq!(client.get_score_history(&asset_id).len(), 5);
    }

    #[test]
    fn test_score_trend_returns_last_n() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        for _ in 0..5 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "entry"),
                &engineer,
            );
            env.ledger().with_mut(|li| li.timestamp += 1);
        }

        let full = client.get_score_history(&asset_id);
        let trend = client.get_score_trend(&asset_id, &3);
        assert_eq!(trend.len(), 3);
        // Should be the last 3 entries
        assert_eq!(trend.get(0).unwrap().score, full.get(2).unwrap().score);
        assert_eq!(trend.get(1).unwrap().score, full.get(3).unwrap().score);
        assert_eq!(trend.get(2).unwrap().score, full.get(4).unwrap().score);
    }

    #[test]
    fn test_score_trend_n_exceeds_history_length() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "only one"),
            &engineer,
        );

        // n=10 but only 1 entry exists ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â should return all 1
        let trend = client.get_score_trend(&asset_id, &10);
        assert_eq!(trend.len(), 1);
    }

    #[test]
    fn test_score_trend_n_zero_returns_empty() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "entry"),
            &engineer,
        );

        let trend = client.get_score_trend(&asset_id, &0);
        assert_eq!(trend.len(), 0);
    }

    #[test]
    fn test_score_trend_empty_history() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);

        let trend = client.get_score_trend(&asset_id, &5);
        assert_eq!(trend.len(), 0);
    }

    #[test]
    fn test_batch_submit_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "Oil change"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("INSPECT"),
            notes: String::from_str(&env, "Inspection"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("ENGINE"),
            notes: String::from_str(&env, "Engine repair"),
        });

        client.batch_submit_maintenance(&asset_id, &records, &engineer);

        // 3 records at default score_increment (5) each => 15
        assert_eq!(client.get_collateral_score(&asset_id), 15);
        assert_eq!(client.get_maintenance_history(&asset_id).len(), 3);
    }

    #[test]
    fn test_batch_submit_maintenance_emits_maint_events() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "Oil change"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("INSPECT"),
            notes: String::from_str(&env, "Inspection"),
        });

        client.batch_submit_maintenance(&asset_id, &records, &engineer);

        let events = env.events().all();
        let maint_count = events
            .iter()
            .filter(|(_, topics, _)| {
                let t0: Result<Symbol, _> = topics.get(0).unwrap().try_into_val(&env);
                t0.map(|s| s == EVENT_MAINT).unwrap_or(false)
            })
            .count();
        // One MAINT event per record submitted
        assert_eq!(maint_count, 2);
    }

    #[test]
    fn test_batch_submit_maintenance_rejects_unknown_task_type() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("UNKNOWN"),
            notes: String::from_str(&env, "Unknown task type"),
        });

        let result = client.try_batch_submit_maintenance(&asset_id, &records, &engineer);

        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidTaskType as u32,
            ))),
        );
    }

    #[test]
    fn test_batch_submit_no_duplicate_engineer_history() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Submit multiple records for the same asset in one batch
        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "Oil change 1"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "Oil change 2"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("INSPECT"),
            notes: String::from_str(&env, "Inspection"),
        });

        client.batch_submit_maintenance(&asset_id, &records, &engineer);

        // Verify engineer history contains asset_id only once
        let history = client.get_engineer_maintenance_history(&engineer);
        let asset_count = history.iter().filter(|id| *id == asset_id).count();
        assert_eq!(asset_count, 1);
    }

    #[test]
    fn test_batch_submit_no_partial_writes_on_failure() {
        let env = Env::default();
        env.mock_all_auths();

        // max_history = 0 means unlimited
        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let asset_id = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);

        // First record is valid; second has an invalid task type — batch must fail cleanly.
        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "valid"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("INVALID"),
            notes: String::from_str(&env, "bad task"),
        });

        let result = client.try_batch_submit_maintenance(&asset_id, &records, &engineer);
        assert!(result.is_err(), "batch should fail on invalid task type");

        // Neither maintenance history nor score history should have any entries.
        assert_eq!(
            client.get_maintenance_history(&asset_id).len(),
            0,
            "HIST must be empty after failed batch"
        );
        assert_eq!(
            client.get_score_history(&asset_id).len(),
            0,
            "SCHIST must be empty after failed batch"
        );
    }

    #[test]
    fn test_batch_submit_fails_atomically_on_history_cap() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 3);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Fill to max_history - 1 = 2
        for _ in 0..2 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "ok"),
                &engineer,
            );
        }
        assert_eq!(client.get_maintenance_history(&asset_id).len(), 2);

        // Batch of 2 would push total to 4, exceeding cap of 3
        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "ok"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "ok"),
        });

        let result = client.try_batch_submit_maintenance(&asset_id, &records, &engineer);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::HistoryCapReached as u32,
            ))),
        );

        // No records written ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â history still at 2
        assert_eq!(client.get_maintenance_history(&asset_id).len(), 2);
    }

    #[test]
    fn test_batch_submit_exceeds_history_cap() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 2);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "First"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "Second"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "Third - over cap"),
        });

        let result = client.try_batch_submit_maintenance(&asset_id, &records, &engineer);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::HistoryCapReached as u32,
            ))),
        );
    }

    #[test]
    fn test_batch_submit_unauthorized_engineer() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let unregistered = Address::generate(&env);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "Should fail"),
        });

        let result = client.try_batch_submit_maintenance(&asset_id, &records, &unregistered);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedEngineer as u32,
            ))),
        );
    }

    #[test]
    fn test_batch_submit_reports_failing_record_index() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "Valid"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("INSPECT"),
            notes: String::from_str(&env, "Valid"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("UNKNOWN"),
            notes: String::from_str(&env, "Invalid task type"),
        });

        let result = client.try_batch_submit_maintenance(&asset_id, &records, &engineer);

        // Should fail with InvalidTaskType at index 2
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidTaskType as u32,
            ))),
        );
    }

    #[test]
    fn test_batch_submit_maintenance_rejects_oversized_notes() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, &"x".repeat(300)),
        });

        let result = client.try_batch_submit_maintenance(&asset_id, &records, &engineer);

        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotesTooLong as u32,
            ))),
        );
    }

    #[test]
    fn test_submit_maintenance_unregistered_engineer_should_panic() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let unregistered = Address::generate(&env);

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Should fail"),
            &unregistered,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedEngineer as u32,
            ))),
        );
    }

    #[test]
    fn test_collateral_score_caps_at_100() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // FILTER = 5 points each; 25 submissions would be 125 without a cap
        for _ in 0..25 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("FILTER"),
                &String::from_str(&env, "Filter replacement"),
                &engineer,
            );
        }

        assert_eq!(client.get_collateral_score(&asset_id), 100);
    }

    #[test]
    fn test_submit_maintenance_revoked_engineer_should_panic() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        engineer_registry_client.revoke_credential(&engineer);

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Post-revocation attempt"),
            &engineer,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedEngineer as u32,
            ))),
        );
    }

    #[test]
    fn test_submit_maintenance_revoked_engineer_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        assert!(engineer_registry_client.verify_engineer(&engineer).unwrap_or(false));
        engineer_registry_client.revoke_credential(&engineer);
        assert!(!engineer_registry_client.verify_engineer(&engineer).unwrap_or(true));

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Post-revocation attempt"),
            &engineer,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedEngineer as u32,
            ))),
        );
    }

    /// Issue #128: revoked engineer cannot submit, but can after re-registration with a new credential.
    #[test]
    fn test_submit_maintenance_revoked_then_reregistered_engineer() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);

        // Set up a trusted issuer and register the engineer
        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let admin = Address::generate(&env);
        let hash_v1 = BytesN::from_array(&env, &[1u8; 32]);

        engineer_registry_client.initialize_admin(&admin, &admin);
        engineer_registry_client.add_trusted_issuer(&admin, &issuer);
        engineer_registry_client.register_engineer(&engineer, &hash_v1, &issuer, &31_536_000);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Revoke the credential
        engineer_registry_client.revoke_credential(&engineer);
        assert_eq!(engineer_registry_client.verify_engineer(&engineer), Some(false));

        // Attempt to submit maintenance ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â must fail with UnauthorizedEngineer
        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Post-revocation attempt"),
            &engineer,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedEngineer as u32,
            ))),
        );

        // Re-register the same engineer with a new credential hash
        let hash_v2 = BytesN::from_array(&env, &[2u8; 32]);
        engineer_registry_client.register_engineer(&engineer, &hash_v2, &issuer, &31_536_000);
        assert_eq!(engineer_registry_client.verify_engineer(&engineer), Some(true));

        // Submission must now succeed
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Post-reregistration submission"),
            &engineer,
        );

        let history = client.get_maintenance_history(&asset_id);
        assert_eq!(history.len(), 1);
        assert_eq!(history.get(0).unwrap().engineer, engineer);
    }

    #[test]
    fn test_submit_maintenance_expired_engineer_should_panic() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);

        // Register engineer with minimum validity period (86400 seconds)
        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let admin = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);
        engineer_registry_client.initialize_admin(&admin, &admin);
        engineer_registry_client.add_trusted_issuer(&admin, &issuer);
        engineer_registry_client.register_engineer(&engineer, &hash, &issuer, &86_400);

        // Verify engineer is initially valid
        assert_eq!(engineer_registry_client.verify_engineer(&engineer), Some(true));

        // Advance ledger past expiry (86401 seconds)
        env.ledger()
            .with_mut(|li| li.timestamp = li.timestamp + 86_401);

        // Verify engineer is now expired
        assert_eq!(engineer_registry_client.verify_engineer(&engineer), Some(false));

        // Attempt submit_maintenance and assert UnauthorizedEngineer is returned
        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Post-expiry attempt"),
            &engineer,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedEngineer as u32,
            ))),
        );
    }

    #[test]
    fn test_submit_maintenance_rejects_expired_credential_via_cross_contract_call() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);

        // Register asset
        let owner = Address::generate(&env);
        let asset_id = asset_registry_client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Test Generator"),
            &unique_serial(&env),
            &owner,
        );

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let eng_admin = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);
        engineer_registry_client.initialize_admin(&eng_admin, &eng_admin);
        engineer_registry_client.add_trusted_issuer(&eng_admin, &issuer);
        // Register with validity_period = 86400 seconds (minimum)
        engineer_registry_client.register_engineer(&engineer, &hash, &issuer, &86_400);

        assert_eq!(engineer_registry_client.verify_engineer(&engineer), Some(true));

        // Advance ledger by 101 seconds ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â credential is now expired
        env.ledger().with_mut(|li| li.timestamp += 86_401);

        assert_eq!(engineer_registry_client.verify_engineer(&engineer), Some(false));

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Post-expiry attempt"),
            &engineer,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedEngineer as u32,
            ))),
        );
    }

    #[test]
    fn test_batch_submit_maintenance_rejects_expired_credential_via_cross_contract_call() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);

        // Register asset
        let owner = Address::generate(&env);
        let asset_id = asset_registry_client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Test Generator"),
            &unique_serial(&env),
            &owner,
        );

        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let eng_admin = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1u8; 32]);
        engineer_registry_client.initialize_admin(&eng_admin, &eng_admin);
        engineer_registry_client.add_trusted_issuer(&eng_admin, &issuer);
        // Register with validity_period = 86400 seconds (minimum)
        engineer_registry_client.register_engineer(&engineer, &hash, &issuer, &86_400);

        assert_eq!(engineer_registry_client.verify_engineer(&engineer), Some(true));

        // Advance ledger by 101 seconds ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â credential is now expired
        env.ledger().with_mut(|li| li.timestamp += 86_401);

        assert_eq!(engineer_registry_client.verify_engineer(&engineer), Some(false));

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "Post-expiry batch attempt"),
        });

        let result = client.try_batch_submit_maintenance(&asset_id, &records, &engineer);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedEngineer as u32,
            ))),
        );
    }

    #[test]
    fn test_full_lifecycle_integration() {
        let env = Env::default();
        env.mock_all_auths();

        let (lifecycle, asset_registry, engineer_registry, _) = setup(&env, 0);

        // 1. Register asset
        let asset_admin = asset_registry.get_admin();
        asset_registry.add_asset_type(&asset_admin, &symbol_short!("TURBINE"));
        let owner = Address::generate(&env);
        let asset_id = asset_registry.register_asset(
            &symbol_short!("TURBINE"),
            &String::from_str(&env, "GE LM2500 Turbine Unit 7"),
            &unique_serial(&env),
            &owner,
        );
        let asset = asset_registry.get_asset(&asset_id);
        assert_eq!(asset.owner, owner);

        // 2. Register and verify engineer
        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let admin = Address::generate(&env);
        engineer_registry.initialize_admin(&admin, &admin);
        engineer_registry.add_trusted_issuer(&admin, &issuer);
        engineer_registry.register_engineer(
            &engineer,
            &BytesN::from_array(&env, &[2u8; 32]),
            &issuer,
            &31_536_000,
        );
        assert_eq!(engineer_registry.verify_engineer(&engineer), Some(true));

        // 3. Submit 10 maintenance records (default score_increment = 5pts each)
        for i in 0..10u32 {
            lifecycle.submit_maintenance(
                &asset_id,
                &symbol_short!("ENGINE"),
                &String::from_str(&env, "Full engine service"),
                &engineer,
            );
            // advance ledger timestamp so records are distinct
            env.ledger().set_timestamp(env.ledger().timestamp() + 1);
            let _ = i;
        }

        // 4. Assert collateral eligible (score >= 50)
        assert!(lifecycle.is_collateral_eligible(&asset_id));

        // 5. Assert get_last_service returns the correct record
        let last = lifecycle.get_last_service(&asset_id).unwrap();
        assert_eq!(last.asset_id, asset_id);
        assert_eq!(last.engineer, engineer);
        assert_eq!(last.task_type, symbol_short!("ENGINE"));
    }

    #[test]
    fn test_decay_score_emits_correct_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Default score_increment = 5
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "ok"),
            &engineer,
        );
        let initial_score: u32 = 5;

        // Use fast decay: 2 pts per 60s, advance 60s (1 interval)
        client.update_decay_config(&admin, &2, &60);
        env.ledger().with_mut(|li| li.timestamp += 60);
        let decay_time = env.ledger().timestamp();

        client.decay_score(&asset_id);

        let events = env.events().all();
        assert!(events.len() >= 1);

        let (_, topics, data) = events.get(0).unwrap();

        // Topics: (symbol("DECAY"), asset_id)
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        let t1: u64 = topics.get(1).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, EVENT_DECAY);
        assert_eq!(t1, asset_id);

        // Data: (old_score, new_score, timestamp)
        let expected_new_score: u32 = initial_score - 2;
        let (ev_old, ev_new, ev_ts): (u32, u32, u64) = data.try_into_val(&env).unwrap();
        assert_eq!(ev_old, initial_score);
        assert_eq!(ev_new, expected_new_score);
        assert_eq!(ev_ts, decay_time);
    }

    #[test]
    fn test_admin_can_reset_score() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build up a non-zero score
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Major overhaul"),
            &engineer,
        );
        assert!(client.get_collateral_score(&asset_id) > 0);

        // Admin resets the score
        client.reset_score(&admin, &asset_id);
        assert_eq!(client.get_collateral_score(&asset_id), 0);
    }

    #[test]
    fn test_reset_score_appends_zero_to_score_history() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Major overhaul"),
            &engineer,
        );
        let history_before = client.get_score_history(&asset_id);
        assert!(history_before.len() > 0);
        assert!(history_before.last_unchecked().score > 0);

        client.reset_score(&admin, &asset_id);

        let history_after = client.get_score_history(&asset_id);
        assert_eq!(history_after.len(), history_before.len() + 1);
        assert_eq!(history_after.last_unchecked().score, 0);
    }

    #[test]
    fn test_decay_after_reset_uses_reset_timestamp() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build up a score, then reset
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Major overhaul"),
            &engineer,
        );
        client.reset_score(&admin, &asset_id);

        // Rebuild score after reset
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Post-reset work"),
            &engineer,
        );
        let score_after_rebuild = client.get_collateral_score(&asset_id);
        assert!(score_after_rebuild > 0);

        // Advance time by less than one decay interval (default 2592000s / 30 days)
        // so no decay should be applied
        env.ledger().with_mut(|li| li.timestamp += 100);
        let score_after_short_wait = client.decay_score(&asset_id);
        assert_eq!(score_after_short_wait, score_after_rebuild);

        // Advance time by one full decay interval and verify exactly one decay step
        env.ledger().with_mut(|li| li.timestamp += 2592000);
        let score_after_decay = client.decay_score(&asset_id);
        assert_eq!(score_after_decay, score_after_rebuild.saturating_sub(5));
    }

    #[test]
    fn test_task_weight_tiers() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Minor: OIL_CHG — score increments by score_increment (default 5)
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "ok"),
            &engineer,
        );
        assert_eq!(client.get_collateral_score(&asset_id), 5);

        client.reset_score(&admin, &asset_id);

        // Medium: FILTER — score increments by score_increment (default 5)
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("FILTER"),
            &String::from_str(&env, "ok"),
            &engineer,
        );
        assert_eq!(client.get_collateral_score(&asset_id), 5);

        client.reset_score(&admin, &asset_id);

        // Major: ENGINE — score increments by score_increment (default 5)
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "ok"),
            &engineer,
        );
        assert_eq!(client.get_collateral_score(&asset_id), 5);

        client.reset_score(&admin, &asset_id);

        let result = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("UNKNOWN"),
            &String::from_str(&env, "ok"),
            &engineer,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidTaskType as u32,
            ))),
        );
    }

    #[test]
    fn test_reset_score_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build up a non-zero score
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Major overhaul"),
            &engineer,
        );

        // Reset the score
        let reset_time = env.ledger().timestamp();
        client.reset_score(&admin, &asset_id);

        // Verify the reset event was emitted
        let events = env.events().all();
        let (_, topics, data) = events.last().unwrap();

        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        let t1: u64 = topics.get(1).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, EVENT_RST_SCR);
        assert_eq!(t1, asset_id);

        let (emitted_admin, emitted_timestamp): (Address, u64) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, admin);
        assert_eq!(emitted_timestamp, reset_time);
    }

    #[test]
    fn test_non_admin_cannot_reset_score() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Major overhaul"),
            &engineer,
        );

        let outsider = Address::generate(&env);
        let result = client.try_reset_score(&outsider, &asset_id);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    // --- get_last_service_timestamp tests ---

    #[test]
    fn test_get_last_service_timestamp_none_before_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);

        assert_eq!(client.get_last_service_timestamp(&asset_id), None);
    }

    #[test]
    fn test_get_last_service_timestamp_returns_ledger_time() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let t0 = env.ledger().timestamp();
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "first service"),
            &engineer,
        );
        assert_eq!(client.get_last_service_timestamp(&asset_id), Some(t0));

        env.ledger().with_mut(|li| li.timestamp += 500);
        let t1 = env.ledger().timestamp();
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("FILTER"),
            &String::from_str(&env, "second service"),
            &engineer,
        );
        assert_eq!(client.get_last_service_timestamp(&asset_id), Some(t1));
    }

    #[test]
    fn test_get_score_history_nonexistent_asset_returns_empty() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let result = client.get_score_history(&999u64);
        assert_eq!(
            result.len(),
            0,
            "nonexistent asset should return empty history"
        );
    }

    #[test]
    fn test_get_score_trend_nonexistent_asset_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let result = client.try_get_score_trend(&999u64, &10u32);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_get_last_service_timestamp_nonexistent_asset_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let result = client.try_get_last_service_timestamp(&999u64);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_get_engineer_maintenance_history_caps_at_100() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let engineer = register_engineer(&env, &engineer_registry_client);

        // Register and maintain 150 assets
        for _ in 0..150 {
            let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "maintenance"),
                &engineer,
            );
        }

        let history = client.get_engineer_maintenance_history(&engineer);
        assert_eq!(history.len(), 100u32);
    }

    #[test]
    fn test_get_engineer_maintenance_history_truncates_at_101() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let engineer = register_engineer(&env, &engineer_registry_client);

        for _ in 0..101 {
            let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
            client.authorize_engineer(&asset_owner, &asset_id, &engineer);
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "maintenance"),
                &engineer,
            );
        }

        let history = client.get_engineer_maintenance_history(&engineer);
        assert_eq!(history.len(), 100u32);
        // confirm the full count is accessible via the count helper
        assert_eq!(client.get_eng_maint_hist_count(&engineer), 101u32);
        assert_eq!(client.get_eng_maint_count(&engineer), 101u32);
        assert_eq!(client.eng_maintenance_history_count(&engineer), 101u32);
    }

    #[test]
    fn test_get_engineer_maintenance_history_returns_all_if_under_100() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let engineer = register_engineer(&env, &engineer_registry_client);

        // Register and maintain 50 assets
        for _ in 0..50 {
            let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
            client.authorize_engineer(&asset_owner, &asset_id, &engineer);
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "maintenance"),
                &engineer,
            );
        }

        let history = client.get_engineer_maintenance_history(&engineer);
        assert_eq!(history.len(), 50u32);
    }

    // --- Issue #142: NotInitialized structured error ---

    #[test]
    fn test_registry_addresses_survive_instance_ttl_boundary() {
        let env = Env::default();
        env.mock_all_auths();

        let asset_registry_id = env.register(AssetRegistry, ());
        let engineer_registry_id = env.register(EngineerRegistry, ());
        let lifecycle_id = env.register(Lifecycle, ());
        let admin = Address::generate(&env);

        let lifecycle = LifecycleClient::new(&env, &lifecycle_id);
        lifecycle.initialize(
            &admin,
            &asset_registry_id,
            &engineer_registry_id,
            &admin,
            &0u32,
        );

        // Verify registries are accessible normally
        assert_eq!(lifecycle.get_asset_registry(), asset_registry_id);
        assert_eq!(lifecycle.get_engineer_registry(), engineer_registry_id);

        // Simulate instance TTL expiration by clearing instance storage keys
        env.as_contract(&lifecycle_id, || {
            env.storage().instance().remove(&CONFIG);
        });

        // After instance TTL expiry, registry addresses should still be readable
        // from persistent storage even though CONFIG is gone
        let asset_reg_persisted: Option<Address> = env.as_contract(&lifecycle_id, || {
            env.storage().persistent().get(&ASSET_REGISTRY)
        });
        let eng_reg_persisted: Option<Address> = env.as_contract(&lifecycle_id, || {
            env.storage().persistent().get(&ENG_REGISTRY)
        });

        assert_eq!(asset_reg_persisted, Some(asset_registry_id));
        assert_eq!(eng_reg_persisted, Some(engineer_registry_id));
    }

    #[test]
    fn test_get_collateral_score_before_init_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();

        // Deploy lifecycle without calling initialize
        let lifecycle_id = env.register(Lifecycle, ());
        let client = LifecycleClient::new(&env, &lifecycle_id);

        let result = client.try_get_collateral_score(&1u64);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotInitialized as u32,
            ))),
        );
    }

    #[test]
    fn test_get_asset_registry_before_init_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();

        let lifecycle_id = env.register(Lifecycle, ());
        let client = LifecycleClient::new(&env, &lifecycle_id);

        let result = client.try_get_asset_registry();
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotInitialized as u32,
            ))),
        );
    }

    #[test]
    fn test_get_engineer_registry_before_init_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();

        let lifecycle_id = env.register(Lifecycle, ());
        let client = LifecycleClient::new(&env, &lifecycle_id);

        let result = client.try_get_engineer_registry();
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotInitialized as u32,
            ))),
        );
    }

    #[test]
    fn test_get_config_before_init_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();

        let lifecycle_id = env.register(Lifecycle, ());
        let client = LifecycleClient::new(&env, &lifecycle_id);

        let result = client.try_get_config();
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotInitialized as u32,
            ))),
        );
    }

    #[test]
    fn test_update_asset_registry_before_init_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();

        let lifecycle_id = env.register(Lifecycle, ());
        let client = LifecycleClient::new(&env, &lifecycle_id);
        let admin = Address::generate(&env);
        let new_registry = Address::generate(&env);

        let result = client.try_update_asset_registry(&admin, &new_registry);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotInitialized as u32,
            ))),
        );
    }

    #[test]
    fn test_update_engineer_registry_before_init_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();

        let lifecycle_id = env.register(Lifecycle, ());
        let client = LifecycleClient::new(&env, &lifecycle_id);
        let admin = Address::generate(&env);
        let new_registry = Address::generate(&env);

        let result = client.try_update_engineer_registry(&admin, &new_registry);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotInitialized as u32,
            ))),
        );
    }

    #[test]
    fn test_update_asset_registry_emits_reg_ast_topic() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let new_registry = Address::generate(&env);

        client.update_asset_registry(&admin, &new_registry);

        let events = env.events().all();
        assert!(events.len() >= 1);

        let (_, topics, _data) = events.get(0).unwrap();
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, EVENT_REG_AST);
    }

    #[test]
    fn test_update_asset_registry_rejects_same_address_as_engineer_registry() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, engineer_registry_client, admin) = setup(&env, 0);
        let eng_registry_addr = engineer_registry_client.address.clone();

        let result = client.try_update_asset_registry(&admin, &eng_registry_addr);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::SameRegistryAddress as u32,
            ))),
        );
    }

    #[test]
    fn test_update_engineer_registry_emits_reg_eng_topic() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let new_registry = Address::generate(&env);

        client.update_engineer_registry(&admin, &new_registry);

        let events = env.events().all();
        assert!(events.len() >= 1);

        let (_, topics, _data) = events.get(0).unwrap();
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, EVENT_REG_ENG);
    }

    #[test]
    fn test_update_asset_registry_zero_address_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let zero = Address::from_str(
            &env,
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
        );

        let result = client.try_update_asset_registry(&admin, &zero);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::ZeroAddress as u32,
            ))),
        );
    }

    #[test]
    fn test_update_engineer_registry_zero_address_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let zero = Address::from_str(
            &env,
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
        );

        let result = client.try_update_engineer_registry(&admin, &zero);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::ZeroAddress as u32,
            ))),
        );
    }

    #[test]
    fn test_update_asset_registry_same_as_engineer_registry_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, engineer_registry_client, admin) = setup(&env, 0);
        let eng_addr = engineer_registry_client.address.clone();

        let result = client.try_update_asset_registry(&admin, &eng_addr);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::SameRegistryAddress as u32,
            ))),
        );
    }

    #[test]
    fn test_update_engineer_registry_same_as_asset_registry_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, admin) = setup(&env, 0);
        let asset_addr = asset_registry_client.address.clone();

        let result = client.try_update_engineer_registry(&admin, &asset_addr);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::SameRegistryAddress as u32,
            ))),
        );
    }

    // --- Issue #144: batch_submit_maintenance updates score_history_key ---

    #[test]
    fn test_batch_submit_score_history_length_matches_records() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "First"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("INSPECT"),
            notes: String::from_str(&env, "Second"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("ENGINE"),
            notes: String::from_str(&env, "Third"),
        });

        client.batch_submit_maintenance(&asset_id, &records, &engineer);

        let score_history = client.get_score_history(&asset_id);
        // All 3 batch records share the same ledger timestamp, so score_history_push
        // deduplicates them into a single entry containing the final score.
        assert_eq!(
            score_history.len(),
            1,
            "batch records in the same ledger should produce exactly 1 score history entry"
        );
    }

    #[test]
    fn test_batch_submit_extends_ttl() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "ttl test"),
        });
        client.batch_submit_maintenance(&asset_id, &records, &engineer);

        let contract_id = client.address.clone();
        env.as_contract(&contract_id, || {
            assert!(env.storage().persistent().get_ttl(&history_key(asset_id)) > 0);
            assert!(env.storage().persistent().get_ttl(&score_key(asset_id)) > 0);
            assert!(
                env.storage()
                    .persistent()
                    .get_ttl(&score_history_key(asset_id))
                    > 0
            );
            assert!(
                env.storage()
                    .persistent()
                    .get_ttl(&last_update_key(asset_id))
                    > 0
            );
        });
    }

    #[test]
    fn test_get_maintenance_history_page() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        for _ in 0..5 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "oil change"),
                &engineer,
            );
        }

        // First page: offset=0, limit=2 ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ 2 records
        assert_eq!(
            client.get_maintenance_history_page(&asset_id, &0, &2).len(),
            2
        );
        // Second page: offset=2, limit=2 ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ 2 records
        assert_eq!(
            client.get_maintenance_history_page(&asset_id, &2, &2).len(),
            2
        );
        // Third page: offset=4, limit=2 ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ 1 record (only one left)
        assert_eq!(
            client.get_maintenance_history_page(&asset_id, &4, &2).len(),
            1
        );
        // Out-of-bounds offset -> empty vec
        assert_eq!(
            client
                .get_maintenance_history_page(&asset_id, &10, &2)
                .len(),
            0
        );
        // limit=0 -> empty
        assert_eq!(
            client.get_maintenance_history_page(&asset_id, &0, &0).len(),
            0
        );
    }

    #[test]
    fn test_get_maintenance_history_page_out_of_bounds() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        for _ in 0..3 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "oil change"),
                &engineer,
            );
        }

        // offset == len (3) -> empty vec
        let result = client.get_maintenance_history_page(&asset_id, &3, &2);
        assert_eq!(result.len(), 0);

        // offset >> len (10) -> empty vec
        let result = client.get_maintenance_history_page(&asset_id, &10, &2);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_get_maintenance_history_page_nonexistent_asset_returns_error() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let result = client.try_get_maintenance_history_page(&999u64, &0, &10);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_get_engineer_maintenance_history_page() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let engineer = register_engineer(&env, &engineer_registry_client);

        // Submit maintenance on 5 different assets
        for _ in 0..5 {
            let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "oil change"),
                &engineer,
            );
        }

        // First page: offset=0, limit=2 ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ 2 assets
        assert_eq!(client.get_eng_history_page(&engineer, &0, &2).len(), 2);
        // Second page: offset=2, limit=2 ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ 2 assets
        assert_eq!(client.get_eng_history_page(&engineer, &2, &2).len(), 2);
        // Third page: offset=4, limit=2 ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ 1 asset (only one left)
        assert_eq!(client.get_eng_history_page(&engineer, &4, &2).len(), 1);
        // Out-of-bounds offset ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ empty
        assert_eq!(client.get_eng_history_page(&engineer, &10, &2).len(), 0);
        // limit=0 ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ empty
        assert_eq!(client.get_eng_history_page(&engineer, &0, &0).len(), 0);
    }

    // --- Issue #207: decay_score extends TTL ---

    #[test]
    fn test_decay_score_extends_ttl() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Maintenance"),
            &engineer,
        );

        let score_key = (symbol_short!("SCORE"), asset_id);
        let last_update_key = (symbol_short!("LUPD"), asset_id);
        let score_history_key = (symbol_short!("SCHIST"), asset_id);

        let contract_id = client.address.clone();

        // Verify entries exist before decay
        env.as_contract(&contract_id, || {
            assert!(env.storage().persistent().has(&score_key));
            assert!(env.storage().persistent().has(&last_update_key));
            assert!(env.storage().persistent().has(&score_history_key));
        });

        // Call decay_score
        client.decay_score(&asset_id);

        // Verify entries still exist after decay (TTL was extended)
        env.as_contract(&contract_id, || {
            assert!(env.storage().persistent().has(&score_key));
            assert!(env.storage().persistent().has(&last_update_key));
            assert!(env.storage().persistent().has(&score_history_key));
        });
    }

    // --- Issue #208: submit_maintenance extends TTL ---

    #[test]
    fn test_submit_maintenance_extends_ttl() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let history_key = (symbol_short!("HIST"), asset_id);
        let score_key = (symbol_short!("SCORE"), asset_id);
        let score_history_key = (symbol_short!("SCHIST"), asset_id);
        let last_update_key = (symbol_short!("LUPD"), asset_id);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Maintenance"),
            &engineer,
        );

        let contract_id = client.address.clone();

        // Verify all keys exist and TTL was extended
        env.as_contract(&contract_id, || {
            assert!(env.storage().persistent().has(&history_key));
            assert!(env.storage().persistent().has(&score_key));
            assert!(env.storage().persistent().has(&score_history_key));
            assert!(env.storage().persistent().has(&last_update_key));
        });
    }

    // --- Issue #209: batch_submit_maintenance extends TTL ---

    #[test]
    fn test_batch_submit_maintenance_extends_ttl() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let history_key = (symbol_short!("HIST"), asset_id);
        let score_key = (symbol_short!("SCORE"), asset_id);
        let score_history_key = (symbol_short!("SCHIST"), asset_id);
        let last_update_key = (symbol_short!("LUPD"), asset_id);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "Oil change"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("INSPECT"),
            notes: String::from_str(&env, "Inspection"),
        });

        client.batch_submit_maintenance(&asset_id, &records, &engineer);

        // Verify all keys exist and TTL was extended
        let contract_id = client.address.clone();
        env.as_contract(&contract_id, || {
            assert!(env.storage().persistent().has(&history_key));
            assert!(env.storage().persistent().has(&score_key));
            assert!(env.storage().persistent().has(&score_history_key));
            assert!(env.storage().persistent().has(&last_update_key));
        });
    }

    // --- Issue #396: score_history_push sets TTL on first creation and extends on subsequent writes ---

    #[test]
    fn test_score_history_ttl_set_on_first_creation() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let score_history_key = (symbol_short!("SCHIST"), asset_id);
        let contract_id = client.address.clone();

        // Key must not exist before first maintenance
        env.as_contract(&contract_id, || {
            assert!(!env.storage().persistent().has(&score_history_key));
        });

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "first"),
            &engineer,
        );

        // After first write the key must exist (TTL was set)
        env.as_contract(&contract_id, || {
            assert!(env.storage().persistent().has(&score_history_key));
        });
    }

    #[test]
    fn test_score_history_ttl_extended_on_subsequent_writes() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        let score_history_key = (symbol_short!("SCHIST"), asset_id);
        let contract_id = client.address.clone();

        // First write — creates the entry
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "first"),
            &engineer,
        );

        // Second write — extends TTL on an existing entry
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "second"),
            &engineer,
        );

        env.as_contract(&contract_id, || {
            assert!(env.storage().persistent().has(&score_history_key));
        });
    }

    // --- Issue #210: reset_score extends TTL ---

    #[test]
    fn test_reset_score_extends_ttl() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "Maintenance"),
            &engineer,
        );

        let score_key = (symbol_short!("SCORE"), asset_id);

        // Verify entry exists before reset
        let contract_id = client.address.clone();
        env.as_contract(&contract_id, || {
            assert!(env.storage().persistent().has(&score_key));
        });

        // Call reset_score
        client.reset_score(&admin, &asset_id);

        // Verify entry still exists after reset (TTL was extended)
        env.as_contract(&contract_id, || {
            assert!(env.storage().persistent().has(&score_key));
        });
        assert_eq!(client.get_collateral_score(&asset_id), 0);
    }

    #[test]
    fn test_pause_affects_all_state_changes_in_lifecycle() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        client.pause(&admin);

        // Read-only access should still work while paused
        let score = client.get_collateral_score(&asset_id);
        assert_eq!(score, 0);
        assert!(client.try_get_collateral_score(&asset_id).is_ok());

        // submit_maintenance
        assert_eq!(
            client.try_submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "ok"),
                &engineer
            ),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // batch_submit_maintenance
        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "ok"),
        });
        assert_eq!(
            client.try_batch_submit_maintenance(&asset_id, &records, &engineer),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // decay_score
        assert_eq!(
            client.try_decay_score(&asset_id),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // propose_upgrade
        assert_eq!(
            client.try_propose_upgrade(&admin, &BytesN::from_array(&env, &[0u8; 32])),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );
    }

    #[test]
    fn test_pause_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        client.pause(&admin);

        let events = env.events().all();
        assert!(events.len() >= 1);
        let (_, topics, data) = events.get(0).unwrap();
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("PAUSED"));
        let (emitted_admin,): (Address,) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, admin);
    }

    #[test]
    fn test_unpause_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        client.pause(&admin);
        client.unpause(&admin);

        let events = env.events().all();
        assert!(events.len() >= 1);
        let (_, topics, data) = events.get(0).unwrap();
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("UNPAUSED"));
        let (emitted_admin,): (Address,) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, admin);
    }

    #[test]
    fn test_pause_state_persists_across_ttl_boundary() {
        let env = Env::default();
        env.mock_all_auths();

        let lifecycle_id = env.register(Lifecycle, ());
        let asset_registry_id = env.register(AssetRegistry, ());
        let engineer_registry_id = env.register(EngineerRegistry, ());
        let admin = Address::generate(&env);

        let client = LifecycleClient::new(&env, &lifecycle_id);
        client.initialize(
            &admin,
            &asset_registry_id,
            &engineer_registry_id,
            &admin,
            &0u32,
        );

        // Pause the contract
        client.pause(&admin);
        assert!(client.is_paused());

        // Simulate instance TTL expiry — PAUSED_KEY must survive because it is in persistent storage
        env.as_contract(&lifecycle_id, || {
            // Wipe all instance storage to mimic TTL expiration
            env.storage().instance().remove(&PENDING_ADMIN_KEY);
        });

        // Pause state must still be true after instance TTL boundary
        assert!(
            client.is_paused(),
            "pause state must survive instance TTL expiry"
        );

        // submit_maintenance must still be blocked
        let (_, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);
        assert_eq!(
            client.try_submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "should be blocked"),
                &engineer,
            ),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );
    }

    #[test]
    fn test_engineer_maintenance_history_multiple_assets_and_sessions() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset1, asset1_owner) = register_asset(&env, &asset_registry_client);
        let (asset2, asset2_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset1_owner, &asset1, &engineer);
        client.authorize_engineer(&asset2_owner, &asset2, &engineer);

        client.submit_maintenance(
            &asset1,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Session 1"),
            &engineer,
        );
        // Advance time
        env.ledger().with_mut(|li| li.timestamp += 3600);
        client.submit_maintenance(
            &asset2,
            &symbol_short!("INSPECT"),
            &String::from_str(&env, "Session 2"),
            &engineer,
        );

        let history = client.get_engineer_maintenance_history(&engineer);
        assert_eq!(history.len(), 2);
        assert!(history.contains(&asset1));
        assert!(history.contains(&asset2));
    }

    #[test]
    fn test_is_collateral_eligible_threshold_boundary() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // 9 ÃƒÆ’Ã¢â‚¬â€ FILTER (5 pts each) = 45 ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â below threshold of 50
        for _ in 0..9 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("FILTER"),
                &String::from_str(&env, "Filter replacement"),
                &engineer,
            );
        }
        assert_eq!(client.get_collateral_score(&asset_id), 45);
        assert!(!client.is_collateral_eligible(&asset_id));

        // 1 more FILTER ÃƒÂ¢Ã¢â‚¬Â Ã¢â‚¬â„¢ 50 ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â at threshold, now eligible
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("FILTER"),
            &String::from_str(&env, "Filter replacement"),
            &engineer,
        );
        assert_eq!(client.get_collateral_score(&asset_id), 50);
        assert!(client.is_collateral_eligible(&asset_id));
    }

    // --- Issue #103: initialize rejects zero addresses ---

    #[test]
    fn test_full_cross_contract_integration_with_transfer() {
        let env = Env::default();
        env.mock_all_auths();

        // 1. Set up all three contracts
        let (lifecycle, asset_registry, engineer_registry, _) = setup(&env, 0);

        // 2. Register asset
        let owner = Address::generate(&env);
        let asset_id = asset_registry.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT 3516 Generator"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(asset_registry.get_asset(&asset_id).owner, owner);

        // 3. Register engineer
        let engineer = Address::generate(&env);
        let issuer = Address::generate(&env);
        let eng_admin = Address::generate(&env);
        engineer_registry.initialize_admin(&eng_admin, &eng_admin);
        engineer_registry.add_trusted_issuer(&eng_admin, &issuer);
        engineer_registry.register_engineer(
            &engineer,
            &BytesN::from_array(&env, &[3u8; 32]),
            &issuer,
            &31_536_000,
        );
        assert_eq!(engineer_registry.verify_engineer(&engineer), Some(true));

        // 4. Submit maintenance ÃƒÂ¢Ã¢â€šÂ¬Ã¢â‚¬Â 10 ÃƒÆ’Ã¢â‚¬â€ OVERHAUL (5 pts each) = 50, eligible
        for _ in 0..10 {
            lifecycle.submit_maintenance(
                &asset_id,
                &symbol_short!("OVERHAUL"),
                &String::from_str(&env, "Full overhaul"),
                &engineer,
            );
        }

        // 5. Verify score and collateral eligibility
        assert_eq!(lifecycle.get_collateral_score(&asset_id), 50);
        assert!(lifecycle.is_collateral_eligible(&asset_id));
        assert_eq!(lifecycle.get_maintenance_history(&asset_id).len(), 10);

        // 6. Transfer asset to new owner
        let new_owner = Address::generate(&env);
        asset_registry.transfer_asset(&asset_id, &owner, &new_owner);

        // 7. Verify new owner and that lifecycle state is preserved
        assert_eq!(asset_registry.get_asset(&asset_id).owner, new_owner);
        assert_eq!(lifecycle.get_collateral_score(&asset_id), 50);
        assert!(lifecycle.is_collateral_eligible(&asset_id));
        assert_eq!(
            lifecycle.get_last_service(&asset_id).unwrap().engineer,
            engineer
        );
    }

    #[test]
    fn test_config_survives_ttl_boundary() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);

        // Simulate instance TTL expiry by removing instance-stored keys
        // (Config is in persistent storage, so it should survive)
        env.as_contract(&client.address, || {
            env.storage().instance().remove(&PAUSED_KEY);
        });

        // Config must still be readable and hold the updated value.
        client.update_score_increment(&admin, &10);
        let config = client.get_config();
        assert_eq!(config.score_increment, 10);
    }

    #[test]
    fn test_post_transfer_maintenance_history_access() {
        let env = Env::default();
        env.mock_all_auths();

        let (lifecycle, asset_registry, engineer_registry, _) = setup(&env, 0);

        let owner = Address::generate(&env);
        let new_owner = Address::generate(&env);
        let issuer = Address::generate(&env);
        let engineer = Address::generate(&env);
        let eng_admin = Address::generate(&env);

        engineer_registry.initialize_admin(&eng_admin, &eng_admin);
        engineer_registry.add_trusted_issuer(&eng_admin, &issuer);
        let asset_id = asset_registry.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator GEN-001"),
            &unique_serial(&env),
            &owner,
        );
        engineer_registry.register_engineer(
            &engineer,
            &BytesN::from_array(&env, &[1u8; 32]),
            &issuer,
            &31_536_000,
        );

        // Submit 2 records under original owner
        lifecycle.submit_maintenance(
            &asset_id,
            &symbol_short!("INSPECT"),
            &String::from_str(&env, "Pre-transfer inspection"),
            &engineer,
        );
        lifecycle.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Oil change"),
            &engineer,
        );

        // Transfer asset and record the transfer sentinel
        asset_registry.transfer_asset(&asset_id, &owner, &new_owner);
        lifecycle.record_transfer(&asset_id, &owner, &new_owner);

        // New owner can read full history (2 maintenance + 1 sentinel)
        let history = lifecycle.get_maintenance_history(&asset_id);
        assert_eq!(history.len(), 3);

        // Sentinel is last and marks the ownership boundary
        let sentinel = history.get(2).unwrap();
        assert_eq!(sentinel.task_type, symbol_short!("XFER"));
        assert_eq!(sentinel.engineer, new_owner);

        // Pre-transfer records are still accessible and reference the original engineer
        assert_eq!(history.get(0).unwrap().engineer, engineer);
        assert_eq!(history.get(1).unwrap().engineer, engineer);

        // Score and eligibility are preserved for the new owner
        assert!(lifecycle.get_collateral_score(&asset_id) > 0);
        assert_eq!(asset_registry.get_asset(&asset_id).owner, new_owner);
    }

    /// A non-owner who obtains `new_owner`'s signature for an unrelated transaction
    /// must not be able to replay it to insert a false transfer sentinel.
    /// `record_transfer` must reject any call where `new_owner` is not the current
    /// owner recorded in the asset registry.
    #[test]
    fn test_record_transfer_rejects_non_owner() {
        let env = Env::default();
        env.mock_all_auths();

        let (lifecycle, asset_registry, _, _) = setup(&env, 0);

        let real_owner = Address::generate(&env);
        let attacker = Address::generate(&env);

        // Register an asset under real_owner; ownership stays with real_owner.
        let asset_id = asset_registry.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator NON-OWNER-TEST"),
            &unique_serial(&env),
            &real_owner,
        );

        // Attacker tries to record a transfer claiming they are the new owner,
        // but the registry still shows real_owner as the owner.
        let result = lifecycle.try_record_transfer(&asset_id, &real_owner, &attacker);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedOwner as u32
            )))
        );

        // Confirm no sentinel was written — history must be empty.
        let history = lifecycle.get_maintenance_history(&asset_id);
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn test_record_transfer_event_includes_sentinel_index() {
        let env = Env::default();
        env.mock_all_auths();

        let (lifecycle, asset_registry, engineer_registry, _) = setup(&env, 0);

        let owner = Address::generate(&env);
        let new_owner = Address::generate(&env);
        let issuer = Address::generate(&env);
        let engineer = Address::generate(&env);
        let eng_admin = Address::generate(&env);

        engineer_registry.initialize_admin(&eng_admin, &eng_admin);
        engineer_registry.add_trusted_issuer(&eng_admin, &issuer);
        let asset_id = asset_registry.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator XFER-IDX-001"),
            &unique_serial(&env),
            &owner,
        );
        engineer_registry.register_engineer(
            &engineer,
            &BytesN::from_array(&env, &[2u8; 32]),
            &issuer,
            &31_536_000,
        );

        // Submit 2 records so the sentinel lands at index 2
        lifecycle.submit_maintenance(
            &asset_id,
            &symbol_short!("INSPECT"),
            &String::from_str(&env, "Pre-transfer inspection"),
            &engineer,
        );
        lifecycle.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Oil change"),
            &engineer,
        );

        asset_registry.transfer_asset(&asset_id, &owner, &new_owner);
        lifecycle.record_transfer(&asset_id, &owner, &new_owner);

        let history = lifecycle.get_maintenance_history(&asset_id);
        let expected_index = (history.len() - 1) as u32;

        let events = env.events().all();
        let xfer_event = events
            .iter()
            .find(|(_, topics, _)| {
                topics
                    .get(0)
                    .and_then(|v| v.try_into_val(&env).ok())
                    .map(|s: Symbol| s == symbol_short!("XFER"))
                    .unwrap_or(false)
            })
            .expect("XFER event not emitted");

        let (_, _, data) = xfer_event;
        let (_, _, _, emitted_index): (Address, Address, u64, u32) =
            data.try_into_val(&env).unwrap();

        assert_eq!(emitted_index, expected_index);
    }

    #[test]
    fn test_purge_asset_data_after_deregister() {
        let env = Env::default();
        env.mock_all_auths();

        let (lifecycle, asset_registry, engineer_registry, admin) = setup(&env, 0);

        let owner = Address::generate(&env);
        let issuer = Address::generate(&env);
        let engineer = Address::generate(&env);
        let eng_admin = Address::generate(&env);

        engineer_registry.initialize_admin(&eng_admin, &eng_admin);
        engineer_registry.add_trusted_issuer(&eng_admin, &issuer);
        let asset_id = asset_registry.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator GEN-PURGE-001"),
            &unique_serial(&env),
            &owner,
        );
        engineer_registry.register_engineer(
            &engineer,
            &BytesN::from_array(&env, &[9u8; 32]),
            &issuer,
            &31_536_000,
        );

        lifecycle.submit_maintenance(
            &asset_id,
            &symbol_short!("INSPECT"),
            &String::from_str(&env, "Pre-deregister check"),
            &engineer,
        );

        // Lifecycle data exists before deregister (score history is readable without asset check)
        assert_eq!(lifecycle.get_score_history(&asset_id).len(), 1);

        // Deregister removes asset from registry but lifecycle data persists
        asset_registry.deregister_asset(&owner, &asset_id);
        assert!(
            asset_registry.try_get_asset(&asset_id).is_err(),
            "asset should be gone from registry"
        );
        assert_eq!(
            lifecycle.get_score_history(&asset_id).len(),
            1,
            "score history persists after deregister"
        );

        // purge_asset_data clears all lifecycle storage for the asset
        lifecycle.purge_asset_data(&admin, &asset_id);
        assert_eq!(
            lifecycle.get_score_history(&asset_id).len(),
            0,
            "score history cleared after purge"
        );
    }

    #[test]
    fn test_propose_admin_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let new_admin = Address::generate(&env);

        client.propose_admin(&admin, &new_admin);

        let events = env.events().all();
        assert!(events.iter().any(|(_, topics, _)| {
            topics
                .get(0)
                .and_then(|v| soroban_sdk::TryIntoVal::<_, Symbol>::try_into_val(&v, &env).ok())
                .map(|s| s == symbol_short!("PROP_ADM"))
                .unwrap_or(false)
        }));
    }
    // --- Issue #367: update_decay_config emits CFG_UPD event ---

    #[test]
    fn test_update_decay_config_emits_cfg_upd_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);

        client.update_decay_config(&admin, &10, &120);

        let events = env.events().all();
        assert!(events.len() >= 1);

        let (_, topics, data) = events.get(0).unwrap();
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("CFG_UPD"));

        // Data: (old_decay_rate, new_decay_rate, old_decay_interval, new_decay_interval)
        let (old_rate, new_rate, old_interval, new_interval): (u32, u32, u64, u64) =
            data.try_into_val(&env).unwrap();
        assert_eq!(old_rate, DEFAULT_DECAY_RATE);
        assert_eq!(new_rate, 10u32);
        assert_eq!(old_interval, DEFAULT_DECAY_INTERVAL);
        assert_eq!(new_interval, 120u64);
    }

    #[test]
    fn test_update_decay_config_emits_admin_audit_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let timestamp = env.ledger().timestamp();
        client.update_decay_config(&admin, &10, &120);

        let events = env.events().all();
        let (_, topics, data) = events.last().unwrap();
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        let t1: Symbol = topics.get(1).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("ADM_AUD"));
        assert_eq!(t1, symbol_short!("CFG_UPD"));

        let (emitted_admin, emitted_timestamp, key, rate, interval): (
            Address,
            u64,
            Symbol,
            u32,
            u64,
        ) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, admin);
        assert_eq!(emitted_timestamp, timestamp);
        assert_eq!(key, symbol_short!("DECAY"));
        assert_eq!(rate, 10u32);
        assert_eq!(interval, 120u64);
    }

    #[test]
    fn test_accept_admin_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let new_admin = Address::generate(&env);

        client.propose_admin(&admin, &new_admin);
        client.accept_admin();

        let events = env.events().all();
        let (_, topics, data) = events.last().unwrap();

        use soroban_sdk::TryIntoVal;
        let topic: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(topic, EVENT_ADMIN_SET);

        let emitted_admin: Address = {
            let (a,): (Address,) = data.try_into_val(&env).unwrap();
            a
        };
        assert_eq!(emitted_admin, new_admin);
    }

    // --- Issue #368: update_eligibility_threshold emits CFG_UPD event ---

    #[test]
    fn test_update_eligibility_threshold_emits_cfg_upd_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);

        client.update_eligibility_threshold(&admin, &75);

        let events = env.events().all();
        assert!(events.len() >= 1);

        let (_, topics, data) = events.get(0).unwrap();
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("CFG_UPD"));

        let (old_threshold, new_threshold): (u32, u32) = data.try_into_val(&env).unwrap();
        assert_eq!(old_threshold, DEFAULT_ELIGIBILITY_THRESHOLD);
        assert_eq!(new_threshold, 75u32);
    }

    #[test]
    fn test_update_eligibility_threshold_affects_eligibility() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Build score to 5 (one ENGINE task, score_increment = 5)
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "ok"),
            &engineer,
        );
        assert_eq!(client.get_collateral_score(&asset_id), 5);

        // Default threshold is 50 — not eligible
        assert!(!client.is_collateral_eligible(&asset_id));

        // Lower threshold to 5 — now eligible
        client.update_eligibility_threshold(&admin, &5);
        assert!(client.is_collateral_eligible(&asset_id));
    }

    #[test]
    fn test_non_admin_cannot_update_eligibility_threshold() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let outsider = Address::generate(&env);
        let result = client.try_update_eligibility_threshold(&outsider, &75);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_purge_asset_removes_from_engineer_history() {
        let env = Env::default();
        env.mock_all_auths();

        let (lifecycle, asset_registry, engineer_registry, admin) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry);
        let engineer = register_engineer(&env, &engineer_registry);
        lifecycle.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // Engineer performs maintenance on the asset
        lifecycle.submit_maintenance(
            &asset_id,
            &symbol_short!("INSPECT"),
            &String::from_str(&env, "Routine check"),
            &engineer,
        );

        // Verify asset_id is in engineer's history
        let history = lifecycle.get_eng_history_page(&engineer, &0, &10);
        assert!(history.contains(&asset_id));

        // Purge the asset
        lifecycle.purge_asset_data(&admin, &asset_id);

        // BUG: Currently, asset_id is STILL in engineer's history
        let history_after = lifecycle.get_eng_history_page(&engineer, &0, &10);
        assert!(
            !history_after.contains(&asset_id),
            "Asset ID should be removed from engineer history after purge"
        );
    }

    #[test]
    fn test_initialize_rejects_non_deployer() {
        let env = Env::default();
        let asset_registry_id = env.register(AssetRegistry, ());
        let engineer_registry_id = env.register(EngineerRegistry, ());
        let lifecycle_id = env.register(Lifecycle, ());
        let client = LifecycleClient::new(&env, &lifecycle_id);

        let deployer = Address::generate(&env);
        let attacker = Address::generate(&env);

        use soroban_sdk::IntoVal;
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &attacker,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &lifecycle_id,
                fn_name: "initialize",
                args: (
                    &attacker,
                    &asset_registry_id,
                    &engineer_registry_id,
                    &attacker,
                    &0u32,
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);

        let result = client.try_initialize(
            &deployer,
            &asset_registry_id,
            &engineer_registry_id,
            &attacker,
            &0u32,
        );
        assert!(
            result.is_err(),
            "non-deployer must not be able to initialize"
        );
    }

    /// An asset with a single maintenance record must never score 0, even after enough
    /// time has elapsed for decay to fully consume the raw score.
    #[test]
    fn test_score_floor_with_sparse_history() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&asset_owner, &asset_id, &engineer);

        // One record → raw score = DEFAULT_SCORE_INCREMENT (5)
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "single record"),
            &engineer,
        );
        assert_eq!(client.get_collateral_score(&asset_id), 5);

        // Advance time by 2 full decay intervals (2 × 30 days).
        // Decay = 2 × DEFAULT_DECAY_RATE (5) = 10 > raw score (5), so raw result = 0.
        env.ledger().with_mut(|li| {
            li.timestamp += 2 * 2_592_000; // 60 days
        });

        // Floor must kick in: score is 1, not 0.
        assert_eq!(
            client.get_collateral_score(&asset_id),
            1,
            "asset with maintenance history must not score 0 after decay"
        );
    }

    #[test]
    fn test_collateral_score_never_exceeds_maximum_with_high_volume_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        asset_registry_client.add_asset_type(&admin, &symbol_short!("TURBINE"));

        let owner_a = Address::generate(&env);
        let asset_id_genset = asset_registry_client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Caterpillar 3516"),
            &unique_serial(&env),
            &owner_a,
        );

        let owner_b = Address::generate(&env);
        let asset_id_turbine = asset_registry_client.register_asset(
            &symbol_short!("TURBINE"),
            &String::from_str(&env, "Siemens SGT-800"),
            &unique_serial(&env),
            &owner_b,
        );

        let engineer = register_engineer(&env, &engineer_registry_client);
        client.authorize_engineer(&owner_a, &asset_id_genset, &engineer);
        client.authorize_engineer(&owner_b, &asset_id_turbine, &engineer);

        for i in 0..110 {
            let note = String::from_str(&env, "Record");
            client.submit_maintenance(
                &asset_id_genset,
                &symbol_short!("ENGINE"),
                &note,
                &engineer,
            );
            client.submit_maintenance(
                &asset_id_turbine,
                &symbol_short!("ENGINE"),
                &note,
                &engineer,
            );
        }

        let score_genset = client.get_collateral_score(&asset_id_genset);
        let score_turbine = client.get_collateral_score(&asset_id_turbine);

        assert!(score_genset <= 100, "score must never exceed 100");
        assert!(score_turbine <= 100, "score must never exceed 100");
        assert_eq!(score_genset, 100, "high-volume maintenance should cap at 100");
        assert_eq!(score_turbine, 100, "high-volume maintenance should cap at 100");
    }

    #[test]
    fn test_collateral_score_accounts_for_maintenance_recency() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let (asset_id_recent, asset_owner_recent) = register_asset(&env, &asset_registry_client);
        let (asset_id_old, asset_owner_old) = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);

        // Submit maintenance for both assets at the same time
        let initial_timestamp = env.ledger().timestamp();
        client.submit_maintenance(
            &asset_id_recent,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "recent maintenance"),
            &engineer,
        );
        client.submit_maintenance(
            &asset_id_old,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "old maintenance"),
            &engineer,
        );

        // Both should have same score initially (score_increment = 5 for a recent record)
        let score_initial_recent = client.get_collateral_score(&asset_id_recent);
        let score_initial_old = client.get_collateral_score(&asset_id_old);
        assert_eq!(score_initial_recent, score_initial_old, "identical assets should have same initial score");

        // Advance time by 15 days (half of MAX_AGE_LEDGERS = 30 days)
        // Record age = 15 days → recency_weight = (30 - 15) / 30 = 0.5
        // For asset_id_old: score = 5 * 0.5 = 2.5 → 2
        env.ledger().with_mut(|li| {
            li.timestamp = initial_timestamp + 15 * 86_400; // 15 days later
        });

        let score_after_15d_old = client.get_collateral_score(&asset_id_old);
        assert!(
            score_after_15d_old < score_initial_old,
            "old maintenance should score lower due to age weighting"
        );

        // Add a fresh maintenance record to asset_id_recent to increase score
        client.submit_maintenance(
            &asset_id_recent,
            &symbol_short!("ENGINE"),
            &String::from_str(&env, "recent maintenance again"),
            &engineer,
        );
        let score_after_15d_recent = client.get_collateral_score(&asset_id_recent);
        assert!(
            score_after_15d_recent > score_after_15d_old,
            "asset with recent records should score higher than asset with only old records"
        );

        // Advance another 15 days (total 30 days from initial submissions)
        // Original records should now have zero contribution
        env.ledger().with_mut(|li| {
            li.timestamp = initial_timestamp + 30 * 86_400; // 30 days later
        });

        let score_at_30d_old = client.get_collateral_score(&asset_id_old);
        assert_eq!(
            score_at_30d_old, 0,
            "records older than MAX_AGE_LEDGERS should contribute zero"
        );

        // asset_id_recent still has the second record (15 days old) contributing
        let score_at_30d_recent = client.get_collateral_score(&asset_id_recent);
        assert!(
            score_at_30d_recent > score_at_30d_old,
            "asset with fresher records should score higher"
        );
    }

    #[test]
    fn test_request_loan_zero_threshold_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);

        let result = client.try_request_loan(&asset_id, &0_u32, &1_i128);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidConfig as u32,
            ))),
        );
    }

    #[test]
    fn test_request_loan_zero_amount_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, _, _) = setup(&env, 0);
        let (asset_id, asset_owner) = register_asset(&env, &asset_registry_client);

        let result = client.try_request_loan(&asset_id, &1_u32, &0_i128);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidConfig as u32,
            ))),
        );
    }

    // --- Issue: score_history_push deduplication by timestamp ---

    #[test]
    fn test_score_history_dedup_same_ledger_submit() {
        // Two submit_maintenance calls in the same ledger (same timestamp) must produce
        // only one score history entry (the latest score), not two.
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let asset_id = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);

        // Both submissions happen at the same ledger timestamp.
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "first in ledger"),
            &engineer,
        );
        client.submit_maintenance(
            &asset_id,
            &symbol_short!("INSPECT"),
            &String::from_str(&env, "second in ledger"),
            &engineer,
        );

        let history = client.get_score_history(&asset_id);
        assert_eq!(
            history.len(),
            1,
            "two submissions in the same ledger must produce exactly 1 score history entry"
        );
        // The single entry reflects the score after both submissions.
        assert!(history.get(0).unwrap().score > 0);
    }

    #[test]
    fn test_score_history_dedup_batch_same_ledger() {
        // A batch of records all share the same ledger timestamp; score_history should
        // contain only one entry (the final accumulated score), not one per record.
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let asset_id = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "rec 1"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("INSPECT"),
            notes: String::from_str(&env, "rec 2"),
        });
        records.push_back(BatchRecord {
            task_type: symbol_short!("FILTER"),
            notes: String::from_str(&env, "rec 3"),
        });

        client.batch_submit_maintenance(&asset_id, &records, &engineer);

        let history = client.get_score_history(&asset_id);
        assert_eq!(
            history.len(),
            1,
            "batch records in the same ledger must produce exactly 1 score history entry"
        );
    }

    #[test]
    fn test_score_history_distinct_ledgers_each_recorded() {
        // Submissions across different ledger timestamps each get their own entry.
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, _) = setup(&env, 0);
        let asset_id = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);

        for i in 0..4u64 {
            client.submit_maintenance(
                &asset_id,
                &symbol_short!("OIL_CHG"),
                &String::from_str(&env, "entry"),
                &engineer,
            );
            env.ledger().with_mut(|li| li.timestamp += 1000 * (i + 1));
        }

        assert_eq!(client.get_score_history(&asset_id).len(), 4);
    }

    // --- Issue: update_max_notes_length tests ---

    #[test]
    fn test_admin_can_update_max_notes_length() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        client.update_max_notes_length(&admin, &512);

        let config = client.get_config();
        assert_eq!(config.max_notes_length, 512);
    }

    #[test]
    fn test_non_admin_cannot_update_max_notes_length() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _) = setup(&env, 0);
        let outsider = Address::generate(&env);

        let result = client.try_update_max_notes_length(&outsider, &512);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_update_max_notes_length_zero_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        let result = client.try_update_max_notes_length(&admin, &0);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidConfig as u32,
            ))),
        );
    }

    #[test]
    fn test_update_max_notes_length_enforced_in_submit_maintenance() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let asset_id = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);

        // Reduce max notes length to 10 bytes
        client.update_max_notes_length(&admin, &10);

        // Notes within the new limit are accepted
        let result_ok = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "short"),
            &engineer,
        );
        assert!(result_ok.is_ok());

        // Notes exceeding the new limit are rejected
        let result_err = client.try_submit_maintenance(
            &asset_id,
            &symbol_short!("INSPECT"),
            &String::from_str(&env, "this is too long"),
            &engineer,
        );
        assert_eq!(
            result_err,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotesTooLong as u32,
            ))),
        );
    }

    #[test]
    fn test_update_max_notes_length_enforced_in_batch_submit() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, asset_registry_client, engineer_registry_client, admin) = setup(&env, 0);
        let asset_id = register_asset(&env, &asset_registry_client);
        let engineer = register_engineer(&env, &engineer_registry_client);

        // Reduce max notes length to 10 bytes
        client.update_max_notes_length(&admin, &10);

        let mut records = Vec::new(&env);
        records.push_back(BatchRecord {
            task_type: symbol_short!("OIL_CHG"),
            notes: String::from_str(&env, "this is too long for limit"),
        });

        let result = client.try_batch_submit_maintenance(&asset_id, &records, &engineer);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotesTooLong as u32,
            ))),
        );
    }

    #[test]
    fn test_update_max_notes_length_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, admin) = setup(&env, 0);
        client.update_max_notes_length(&admin, &128);

        let events = env.events().all();
        assert!(events.len() >= 1);
        use soroban_sdk::TryIntoVal;
        let (_, topics, data) = events.get(0).unwrap();
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("UPD_NOTES"));
        let emitted_max: u32 = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_max, 128);
    }

    // --- #752: upgrade timelock tests ---

    #[test]
    fn test_execute_upgrade_before_timelock_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _, admin) = setup(&env, 0);

        let hash = BytesN::from_array(&env, &[0xabu8; 32]);
        client.propose_upgrade(&admin, &hash);

        let result = client.try_execute_upgrade(&admin);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::TimelockNotExpired as u32,
            ))),
        );
    }

    #[test]
    fn test_execute_upgrade_after_timelock_succeeds() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _, admin) = setup(&env, 0);

        let hash = BytesN::from_array(&env, &[0xabu8; 32]);
        client.propose_upgrade(&admin, &hash);

        let base = env.ledger().timestamp();
        env.ledger().set_timestamp(base + TIMELOCK_DELAY_SECS + 1);

        client.execute_upgrade(&admin);
    }

    #[test]
    fn test_execute_upgrade_without_proposal_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _, admin) = setup(&env, 0);

        let result = client.try_execute_upgrade(&admin);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::ProposalNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_propose_upgrade_non_admin_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _, _) = setup(&env, 0);
        let outsider = Address::generate(&env);

        let hash = BytesN::from_array(&env, &[0xabu8; 32]);
        let result = client.try_propose_upgrade(&outsider, &hash);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    // --- #753: revoke_engineer_auth timelock tests ---

    #[test]
    fn test_propose_revoke_engineer_auth_and_execute_after_delay() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, asset_registry, engineer_registry, _admin) = setup(&env, 0);

        let owner = Address::generate(&env);
        let asset_id = register_asset_for_owner(&env, &asset_registry, &owner);
        let engineer = register_engineer(&env, &engineer_registry);

        client.authorize_engineer(&owner, &asset_id, &engineer);

        // Propose revocation
        client.propose_revoke_engineer_auth(&owner, &asset_id, &engineer);

        // Execute before timelock — should fail
        let result = client.try_execute_revoke_engineer_auth(&owner, &asset_id, &engineer);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::TimelockNotExpired as u32,
            ))),
        );

        // Advance past timelock delay (48 hours)
        let base = env.ledger().timestamp();
        env.ledger().set_timestamp(base + TIMELOCK_DELAY_SECS + 1);

        // Execute after delay — should succeed
        client.execute_revoke_engineer_auth(&owner, &asset_id, &engineer);
    }

    #[test]
    fn test_execute_revoke_engineer_auth_without_proposal_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, asset_registry, engineer_registry, _admin) = setup(&env, 0);

        let owner = Address::generate(&env);
        let asset_id = register_asset_for_owner(&env, &asset_registry, &owner);
        let engineer = register_engineer(&env, &engineer_registry);

        client.authorize_engineer(&owner, &asset_id, &engineer);

        // Execute without proposal — should fail
        let result = client.try_execute_revoke_engineer_auth(&owner, &asset_id, &engineer);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::ProposalNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_propose_revoke_engineer_auth_non_owner_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, asset_registry, engineer_registry, _admin) = setup(&env, 0);

        let owner = Address::generate(&env);
        let rogue = Address::generate(&env);
        let asset_id = register_asset_for_owner(&env, &asset_registry, &owner);
        let engineer = register_engineer(&env, &engineer_registry);

        client.authorize_engineer(&owner, &asset_id, &engineer);

        let result = client.try_propose_revoke_engineer_auth(&rogue, &asset_id, &engineer);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedOwner as u32,
            ))),
        );
    }

    #[test]
    fn test_propose_revoke_engineer_auth_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, asset_registry, engineer_registry, _admin) = setup(&env, 0);

        let owner = Address::generate(&env);
        let asset_id = register_asset_for_owner(&env, &asset_registry, &owner);
        let engineer = register_engineer(&env, &engineer_registry);

        client.authorize_engineer(&owner, &asset_id, &engineer);
        client.propose_revoke_engineer_auth(&owner, &asset_id, &engineer);

        let events = env.events().all();
        use soroban_sdk::TryIntoVal;
        let prop_event = events.iter().find(|(_, topics, _)| {
            if let Some(val) = topics.get(0) {
                if let Ok(s) = val.try_into_val::<_, Symbol>(&env) {
                    return s == symbol_short!("PROP_RVK");
                }
            }
            false
        });
        assert!(prop_event.is_some(), "PROP_RVK event must be emitted on propose_revoke_engineer_auth");
    }
    #[test]
    fn test_get_collateral_score_persists_returned_value() {
        let env = Env::default();
        env.mock_all_auths();

        let (lifecycle, asset_registry, eng_registry, _admin) = setup(&env, 10);
        let asset_id = register_asset(&env, &asset_registry);
        let engineer = Address::generate(&env);
        let cred_hash = BytesN::from_array(&env, &[1u8; 32]);
        let issuer = Address::generate(&env);
        eng_registry.register_engineer(&engineer, &cred_hash, &issuer);

        lifecycle.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "oil change"),
            &engineer,
        );

        // Call get_collateral_score and capture returned value.
        let returned = lifecycle.get_collateral_score(&asset_id);

        // Read the persisted SCORE key directly from storage.
        let stored: u32 = env
            .as_contract(&lifecycle.address, || {
                env.storage()
                    .persistent()
                    .get(&score_key(asset_id))
                    .unwrap_or(0u32)
            });

        assert_eq!(
            returned, stored,
            "stored score ({stored}) must match returned score ({returned})"
        );
    }
}
