#![no_std]
use shared::validation::{require_non_empty_vec, require_string_length};
use soroban_sdk::xdr::ToXdr;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, log, panic_with_error, symbol_short,
    Address, BytesN, Env, String, Symbol, Vec,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ContractError {
    AssetNotFound = 1,
    /// Same owner attempted to register an asset with identical metadata.
    DuplicateAsset = 2,
    UnauthorizedAdmin = 3,
    UnauthorizedOwner = 4,
    NotInitialized = 5,
    AdminAlreadyInitialized = 6,
    Paused = 7,
    InvalidAssetType = 8,
    PendingAdminAlreadyExists = 9,
    TypeInUse = 10,
    EmptyMetadata = 11,
    SameOwner = 12,
    TimelockNotExpired = 13,
    ProposalNotFound = 14,
    AssetDecommissioned = 15,
    /// A pending (non-executed) deregister proposal already exists for this asset.
    /// A new proposal cannot overwrite it; wait for the timelock to expire and execute,
    /// or allow the existing proposal to lapse before re-proposing.
    ProposalAlreadyExists = 16,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Asset {
    pub asset_id: u64,
    pub asset_type: Symbol,
    pub metadata: String,
    /// Unique physical serial number of the asset (e.g. manufacturer plate number).
    /// Used as the primary deduplication key so the same machine cannot be registered
    /// twice even if its metadata description differs.
    pub serial_number: String,
    pub owner: Address,
    pub registered_at: u64,
    pub metadata_updated_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetInput {
    pub asset_type: Symbol,
    pub metadata: String,
    pub serial_number: String,
}

/// Paginated result for `get_assets_by_type_paginated`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetTypePage {
    /// Asset IDs for the requested page.
    pub assets: Vec<u64>,
    /// Total number of assets of this type across all pages.
    pub total: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelockProposal {
    pub proposed_at: u64,
    pub executed: bool,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AssetStatus {
    Active = 0,
    Decommissioned = 1,
    UnderMaintenance = 2,
}

const ASSET_COUNT: Symbol = symbol_short!("A_COUNT");
const PAUSED_KEY: Symbol = symbol_short!("PAUSED");
const TIMELOCK_DELAY_SECS: u64 = 48 * 60 * 60;

const ADMIN_KEY: Symbol = symbol_short!("ADMIN");
const ASSET_TYPE_PREFIX: Symbol = symbol_short!("AST_TYPE");
const PENDING_ADMIN_KEY: Symbol = symbol_short!("PADMIN");
const DECOMM_PREFIX: Symbol = symbol_short!("DECOMM");

/// Soroban persistent-storage TTL constants.
/// 1 ledger ≈ 5 seconds → 518_400 ledgers ≈ 30 days.
const TTL_THRESHOLD: u32 = 518_400;
const TTL_TARGET: u32 = 518_400;
pub const DEREG_TOPIC: Symbol = symbol_short!("DEREG");
pub const ADD_TYPE_TOPIC: Symbol = symbol_short!("ADD_TYPE");
pub const RM_TYPE_TOPIC: Symbol = symbol_short!("RM_TYPE");

fn asset_key(id: u64) -> (Symbol, u64) {
    (symbol_short!("ASSET"), id)
}

fn timelock_key(op: Symbol, asset_id: u64) -> (Symbol, Symbol, u64) {
    (symbol_short!("TL_PROP"), op, asset_id)
}

fn require_timelock_ready(env: &Env, op: Symbol, asset_id: u64) {
    let key = timelock_key(op, asset_id);
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

/// Global timelock key for admin-level operations (e.g., upgrade).
fn global_timelock_key(op: Symbol) -> (Symbol, Symbol) {
    (symbol_short!("TL_GLOB"), op)
}

fn require_global_timelock_ready(env: &Env, op: Symbol) {
    let key = global_timelock_key(op);
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

/// Decommissioned flag key: asset_id → bool.
fn decommissioned_key(asset_id: u64) -> (Symbol, u64) {
    (DECOMM_PREFIX, asset_id)
}

/// Deduplication key: (owner, asset_type, sha256(metadata)) → existing asset_id.
/// asset_type is included so same owner+metadata with different type is not erroneously deduplicated.
fn dedup_key(owner: &Address, asset_type: &Symbol, hash: &BytesN<32>) -> (Symbol, Address, Symbol, BytesN<32>) {
    (symbol_short!("DEDUP"), owner.clone(), asset_type.clone(), hash.clone())
}

/// Serial-number dedup key: sha256(serial_number) → existing asset_id.
/// Prevents the same physical machine from being registered twice regardless of metadata.
fn serial_dedup_key(hash: &BytesN<32>) -> (Symbol, BytesN<32>) {
    (symbol_short!("SN_DEDUP"), hash.clone())
}

/// Owner index key: owner → Vec<u64> of asset IDs.
fn owner_index_key(owner: &Address) -> (Symbol, Address) {
    (symbol_short!("OWN_IDX"), owner.clone())
}

/// Asset type allowlist key: asset_type → bool.
fn asset_type_key(asset_type: &Symbol) -> (Symbol, Symbol) {
    (ASSET_TYPE_PREFIX, asset_type.clone())
}

/// Asset type count key: asset_type → u64 (number of registered assets of this type).
fn type_count_key(asset_type: &Symbol) -> (Symbol, Symbol) {
    (symbol_short!("AST_CNT"), asset_type.clone())
}

fn type_count_inc(env: &Env, asset_type: &Symbol) {
    let key = type_count_key(asset_type);
    let count: u64 = env.storage().persistent().get(&key).unwrap_or(0);
    env.storage().persistent().set(&key, &(count + 1));
    env.storage().persistent().extend_ttl(&key, 518400, 518400);
}

fn type_count_dec(env: &Env, asset_type: &Symbol) {
    let key = type_count_key(asset_type);
    let count: u64 = env.storage().persistent().get(&key).unwrap_or(0);
    if count > 0 {
        env.storage().persistent().set(&key, &(count - 1));
        env.storage().persistent().extend_ttl(&key, 518400, 518400);
    }
}

/// Type-to-assets index key: asset_type → Vec<u64> of asset IDs.
fn type_assets_key(asset_type: &Symbol) -> (Symbol, Symbol) {
    (symbol_short!("TYP_IDX"), asset_type.clone())
}

fn type_assets_add(env: &Env, asset_type: &Symbol, asset_id: u64) {
    let key = type_assets_key(asset_type);
    let mut ids: Vec<u64> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| Vec::new(env));
    ids.push_back(asset_id);
    env.storage().persistent().set(&key, &ids);
    env.storage().persistent().extend_ttl(&key, 518400, 518400);
}

fn type_assets_remove(env: &Env, asset_type: &Symbol, asset_id: u64) {
    let key = type_assets_key(asset_type);
    let ids: Vec<u64> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| Vec::new(env));
    let mut updated: Vec<u64> = Vec::new(env);
    for id in ids.iter() {
        if id != asset_id {
            updated.push_back(id);
        }
    }
    env.storage().persistent().set(&key, &updated);
    env.storage().persistent().extend_ttl(&key, 518400, 518400);
}

/// Append an asset ID to the owner's index.
fn owner_index_add(env: &Env, owner: &Address, asset_id: u64) {
    let key = owner_index_key(owner);
    let mut ids: Vec<u64> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| Vec::new(env));
    ids.push_back(asset_id);
    env.storage().persistent().set(&key, &ids);
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
}

/// Remove an asset ID from the owner's index.
fn owner_index_remove(env: &Env, owner: &Address, asset_id: u64) {
    let key = owner_index_key(owner);
    if !env.storage().persistent().has(&key) {
        log!(
            env,
            "owner index missing during remove",
            owner.clone(),
            asset_id
        );
        env.events()
            .publish((symbol_short!("IDX_MISS"), owner.clone()), asset_id);
        return;
    }
    let ids: Vec<u64> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| Vec::new(env));
    let mut updated: Vec<u64> = Vec::new(env);
    for id in ids.iter() {
        if id != asset_id {
            updated.push_back(id);
        }
    }
    if updated.is_empty() {
        env.storage().persistent().remove(&key);
    } else {
        env.storage().persistent().set(&key, &updated);
        env.storage().persistent().extend_ttl(&key, 518400, 518400);
    }
    env.storage().persistent().set(&key, &updated);
    env.storage()
        .persistent()
        .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
}

fn is_paused(env: &Env) -> bool {
    env.storage().persistent().get(&PAUSED_KEY).unwrap_or(false)
}

fn ensure_not_paused(env: &Env) {
    if is_paused(env) {
        panic_with_error!(env, ContractError::Paused);
    }
}

#[contract]
pub struct AssetRegistry;

#[contractimpl]
impl AssetRegistry {
    /// Propose a timelocked deregistration for an asset.
    /// This is the first step in removing an asset from the registry.
    ///
    /// Timelock semantics: after proposing, the caller must wait
    /// `TIMELOCK_DELAY_SECS` (48 hours) before calling
    /// [`execute_deregister_asset`]. A proposal cannot be re-proposed while
    /// a pending (non-executed) proposal already exists for the same asset —
    /// doing so would reset the clock and allow indefinite delay.
    ///
    /// # Arguments
    /// * `caller` - The address initiating the proposal (owner or admin)
    /// * `asset_id` - The unique identifier of the asset to deregister
    ///
    /// # Panics
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    /// - [`ContractError::UnauthorizedOwner`] if the caller is not the asset owner or admin
    /// - [`ContractError::ProposalAlreadyExists`] if a pending proposal already exists
    pub fn propose_deregister_asset(env: Env, caller: Address, asset_id: u64) {
        ensure_not_paused(&env);
        let asset: Asset = env
            .storage()
            .persistent()
            .get(&asset_key(asset_id))
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::AssetNotFound));
        let admin = Self::get_admin(env.clone());
        if caller == admin {
            admin.require_auth();
        } else if caller == asset.owner {
            asset.owner.require_auth();
        } else {
            panic_with_error!(&env, ContractError::UnauthorizedOwner);
        }
        let key = timelock_key(DEREG_TOPIC, asset_id);
        // Block re-proposal if a pending proposal already exists to prevent
        // the owner from resetting the timelock clock indefinitely.
        if let Some(existing) = env
            .storage()
            .persistent()
            .get::<_, TimelockProposal>(&key)
        {
            if !existing.executed {
                panic_with_error!(&env, ContractError::ProposalAlreadyExists);
            }
        }
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

    /// Execute a previously proposed asset deregistration after the timelock expires.
    ///
    /// # Arguments
    /// * `caller` - The address completing the deregistration
    /// * `asset_id` - The unique identifier of the asset to deregister
    ///
    /// # Panics
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    /// - [`ContractError::UnauthorizedOwner`] if the caller is not the asset owner or admin
    /// - [`ContractError::TimelockNotReady`] if the proposal timelock has not yet matured
    pub fn execute_deregister_asset(env: Env, caller: Address, asset_id: u64) {
        require_timelock_ready(&env, DEREG_TOPIC, asset_id);
        Self::deregister_asset(env, caller, asset_id);
    }

    /// Register a new asset with the given type, metadata, and owner.
    ///
    /// # Arguments
    /// * `asset_type` - A Symbol representing the type of asset (e.g., "GENSET", "TURBINE")
    /// * `metadata` - String containing asset metadata and specifications
    /// * `owner` - Address of the asset owner
    ///
    /// # Returns
    /// The unique asset ID assigned to the registered asset
    ///
    /// # Panics
    /// - [`ContractError::DuplicateAsset`] if the same owner tries to register identical metadata
    /// - [`ContractError::InvalidAssetType`] if the asset type is not in the allowlist
    pub fn register_asset(env: Env, asset_type: Symbol, metadata: String, serial_number: String, owner: Address) -> u64 {
        ensure_not_paused(&env);
        owner.require_auth();

        require_string_length(&metadata, "metadata", 256);
        require_string_length(&serial_number, "serial_number", 64);

        // Validate asset type against allowlist
        if !Self::is_valid_asset_type(env.clone(), asset_type.clone()) {
            panic_with_error!(&env, ContractError::InvalidAssetType);
        }

        // Deduplication by serial number: same physical machine cannot be registered twice.
        let sn_bytes = serial_number.clone().to_xdr(&env);
        let sn_hash: BytesN<32> = env.crypto().sha256(&sn_bytes).into();
        let sdk = serial_dedup_key(&sn_hash);
        if env.storage().persistent().has(&sdk) {
            panic_with_error!(&env, ContractError::DuplicateAsset);
        }

        // Secondary dedup: same owner + same metadata hash.
        let meta_bytes = metadata.clone().to_xdr(&env);
        let meta_hash: BytesN<32> = env.crypto().sha256(&meta_bytes).into();
        let dk = dedup_key(&owner, &asset_type, &meta_hash);
        if env.storage().persistent().has(&dk) {
            panic_with_error!(&env, ContractError::DuplicateAsset);
        }

        let id: u64 = env.storage().persistent().get(&ASSET_COUNT).unwrap_or(0) + 1;
        let asset = Asset {
            asset_id: id,
            asset_type: asset_type.clone(),
            metadata,
            serial_number,
            owner: owner.clone(),
            registered_at: env.ledger().timestamp(),
            metadata_updated_at: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&asset_key(id), &asset);
        env.storage()
            .persistent()
            .extend_ttl(&asset_key(id), TTL_THRESHOLD, TTL_TARGET);
        env.storage().persistent().set(&ASSET_COUNT, &id);
        env.storage()
            .persistent()
            .extend_ttl(&ASSET_COUNT, TTL_THRESHOLD, TTL_TARGET);
        env.storage().persistent().set(&dk, &id);
        env.storage().persistent().extend_ttl(&dk, TTL_THRESHOLD, TTL_TARGET);
        env.storage().persistent().set(&sdk, &id);
        env.storage().persistent().extend_ttl(&sdk, TTL_THRESHOLD, TTL_TARGET);

        // Update owner index
        owner_index_add(&env, &owner, id);

        // Increment type count
        type_count_inc(&env, &asset_type);

        // Update type-to-assets index
        type_assets_add(&env, &asset_type, id);

        // Emit asset registration event
        env.events().publish(
            (symbol_short!("reg_asset"),),
            (id, owner.clone(), env.ledger().timestamp()),
        );

        id
    }

    /// Register multiple assets in a single transaction.
    ///
    /// # Arguments
    /// * `owner` - Address of the asset owner
    /// * `assets` - Vec of AssetInput structs
    ///
    /// # Returns
    /// Vec of assigned asset IDs
    pub fn batch_register_assets(env: Env, owner: Address, assets: Vec<AssetInput>) -> Vec<u64> {
        ensure_not_paused(&env);
        owner.require_auth();
        require_non_empty_vec(&assets, "assets");

        let mut ids: Vec<u64> = Vec::new(&env);
        // Track (asset_type, meta_hash) pairs to detect in-batch duplicates
        let mut batch_type_meta: Vec<(Symbol, BytesN<32>)> = Vec::new(&env);
        let mut batch_sn_hashes: Vec<BytesN<32>> = Vec::new(&env);

        let mut next_id: u64 = env.storage().persistent().get(&ASSET_COUNT).unwrap_or(0);

        for asset_in in assets.iter() {
            require_string_length(&asset_in.metadata, "metadata", 256);
            require_string_length(&asset_in.serial_number, "serial_number", 64);
            if !Self::is_valid_asset_type(env.clone(), asset_in.asset_type.clone()) {
                panic_with_error!(&env, ContractError::InvalidAssetType);
            }

            // Serial-number dedup (global)
            let sn_bytes = asset_in.serial_number.clone().to_xdr(&env);
            let sn_hash: BytesN<32> = env.crypto().sha256(&sn_bytes).into();
            if env.storage().persistent().has(&serial_dedup_key(&sn_hash)) {
                panic_with_error!(&env, ContractError::DuplicateAsset);
            }
            for seen in batch_sn_hashes.iter() {
                if seen == sn_hash {
                    panic_with_error!(&env, ContractError::DuplicateAsset);
                }
            }
            batch_sn_hashes.push_back(sn_hash.clone());

            let meta_bytes = asset_in.metadata.clone().to_xdr(&env);
            let meta_hash: BytesN<32> = env.crypto().sha256(&meta_bytes).into();

            if env
                .storage()
                .persistent()
                .has(&dedup_key(&owner, &asset_in.asset_type, &meta_hash))
            {
                panic_with_error!(&env, ContractError::DuplicateAsset);
            }

            for (seen_type, seen_hash) in batch_type_meta.iter() {
                if seen_type == asset_in.asset_type && seen_hash == meta_hash {
                    panic_with_error!(&env, ContractError::DuplicateAsset);
                }
            }
            batch_type_meta.push_back((asset_in.asset_type.clone(), meta_hash.clone()));

            next_id += 1;
            let id = next_id;
            let asset = Asset {
                asset_id: id,
                asset_type: asset_in.asset_type.clone(),
                metadata: asset_in.metadata.clone(),
                serial_number: asset_in.serial_number.clone(),
                owner: owner.clone(),
                registered_at: env.ledger().timestamp(),
                metadata_updated_at: env.ledger().timestamp(),
            };

            env.storage().persistent().set(&asset_key(id), &asset);
            env.storage()
                .persistent()
                .extend_ttl(&asset_key(id), TTL_THRESHOLD, TTL_TARGET);
            env.storage()
                .persistent()
                .set(&dedup_key(&owner, &asset_in.asset_type, &meta_hash), &id);
            env.storage().persistent().extend_ttl(
                &dedup_key(&owner, &asset_in.asset_type, &meta_hash),
                TTL_THRESHOLD,
                TTL_TARGET,
            );
            env.storage().persistent().set(&serial_dedup_key(&sn_hash), &id);
            env.storage().persistent().extend_ttl(
                &serial_dedup_key(&sn_hash),
                TTL_THRESHOLD,
                TTL_TARGET,
            );

            owner_index_add(&env, &owner, id);

            // Increment type count
            type_count_inc(&env, &asset_in.asset_type);

            // Update type-to-assets index
            type_assets_add(&env, &asset_in.asset_type, id);

            env.events().publish(
                (symbol_short!("REG_AST"), id),
                (
                    asset_in.asset_type.clone(),
                    owner.clone(),
                    env.ledger().timestamp(),
                ),
            );

            ids.push_back(id);
        }

        if next_id > env.storage().persistent().get(&ASSET_COUNT).unwrap_or(0) {
            env.storage().persistent().set(&ASSET_COUNT, &next_id);
            env.storage()
                .persistent()
                .extend_ttl(&ASSET_COUNT, TTL_THRESHOLD, TTL_TARGET);
        }

        // Ensure owner index TTL is extended after all batch writes
        if !ids.is_empty() {
            env.storage().persistent().extend_ttl(
                &owner_index_key(&owner),
                TTL_THRESHOLD,
                TTL_TARGET,
            );
        }

        // Emit batch registration event
        if !ids.is_empty() {
            env.events().publish(
                (symbol_short!("BATCH_REG"), owner.clone()),
                (ids.clone(), env.ledger().timestamp()),
            );
        }

        ids
    }

    /// Retrieve an asset by its unique ID.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset to retrieve
    ///
    /// # Returns
    /// The complete Asset struct containing all asset information
    ///
    /// # Panics
    /// - [`ContractError::AssetNotFound`] if no asset exists with the given ID
    pub fn get_asset(env: Env, asset_id: u64) -> Asset {
        env.storage()
            .persistent()
            .get(&asset_key(asset_id))
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::AssetNotFound))
    }

    /// Returns true if an asset with the given ID exists, false otherwise.
    pub fn asset_exists(env: Env, asset_id: u64) -> bool {
        env.storage().persistent().has(&asset_key(asset_id))
    }

    /// Returns the status of an asset (Active, Decommissioned, or UnderMaintenance).
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset
    ///
    /// # Returns
    /// AssetStatus enum: Active if normal, Decommissioned if marked as such,
    /// UnderMaintenance if the asset is marked as under maintenance
    ///
    /// # Panics
    /// - [`ContractError::AssetNotFound`] if no asset exists with the given ID
    pub fn asset_status(env: Env, asset_id: u64) -> AssetStatus {
        // Verify asset exists
        if !Self::asset_exists(env.clone(), asset_id) {
            panic_with_error!(&env, ContractError::AssetNotFound);
        }

        // Check if asset is decommissioned
        let decomm_key = decommissioned_key(asset_id);
        let is_decommissioned: bool = env
            .storage()
            .persistent()
            .get(&decomm_key)
            .unwrap_or(false);

        if is_decommissioned {
            return AssetStatus::Decommissioned;
        }

        // Check if asset is under maintenance
        let maint_key = (symbol_short!("U_MAINT"), asset_id);
        let is_under_maintenance: bool = env
            .storage()
            .persistent()
            .get(&maint_key)
            .unwrap_or(false);

        if is_under_maintenance {
            return AssetStatus::UnderMaintenance;
        }

        AssetStatus::Active
    }

    /// Returns all asset IDs owned by the given address.
    pub fn get_assets_by_owner(env: Env, owner: Address) -> Vec<u64> {
        let key = owner_index_key(&owner);
        let ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));
        if env.storage().persistent().has(&key) {
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
        }
        ids
    }

    /// Returns a paginated list of asset IDs owned by the given address.
    ///
    /// # Arguments
    /// * `owner` - The address of the asset owner
    /// * `page` - Zero-based page index
    /// * `page_size` - Number of asset IDs to return per page
    ///
    /// # Returns
    /// Vec containing the requested page of asset IDs
    pub fn get_assets_by_owner_page(env: Env, owner: Address, page: u32, page_size: u32) -> Vec<u64> {
        let key = owner_index_key(&owner);
        let all_assets: Vec<u64> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));

        if env.storage().persistent().has(&key) {
            env.storage()
                .persistent()
                .extend_ttl(&key, TTL_THRESHOLD, TTL_TARGET);
        }

        if page_size == 0 {
            return Vec::new(&env);
        }

        let len = all_assets.len();
        let offset = match page.checked_mul(page_size) {
            Some(offset) => offset,
            None => return Vec::new(&env),
        };
        if offset >= len {
            return Vec::new(&env);
        }

        let end = offset.checked_add(page_size).unwrap_or(len).min(len);
        let mut page_assets = Vec::new(&env);
        for i in offset..end {
            page_assets.push_back(all_assets.get(i).unwrap());
        }
        page_assets
    }

    /// Get the total count of registered assets in the system.
    ///
    /// # Returns
    /// The total number of assets that have been registered
    pub fn asset_count(env: Env) -> u64 {
        env.storage().persistent().get(&ASSET_COUNT).unwrap_or(0)
    }

    /// Get the total count of registered assets.
    ///
    /// # Returns
    /// The total number of assets that have been registered
    pub fn get_asset_count(env: Env) -> u64 {
        env.storage().persistent().get(&ASSET_COUNT).unwrap_or(0)
    }

    /// Returns all asset IDs of the given type.
    pub fn get_assets_by_type(env: Env, asset_type: Symbol) -> Vec<u64> {
        let key = type_assets_key(&asset_type);
        let ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));
        if env.storage().persistent().has(&key) {
            env.storage().persistent().extend_ttl(&key, 518400, 518400);
        }
        ids
    }

    /// Returns a paginated list of asset IDs of the given type.
    ///
    /// # Arguments
    /// * `asset_type` - The asset type symbol to query
    /// * `offset` - Starting index for pagination
    /// * `limit` - Maximum number of asset IDs to return
    pub fn get_assets_by_type_page(
        env: Env,
        asset_type: Symbol,
        offset: u32,
        limit: u32,
    ) -> Vec<u64> {
        let all: Vec<u64> = env
            .storage()
            .persistent()
            .get(&type_assets_key(&asset_type))
            .unwrap_or_else(|| Vec::new(&env));
        let len = all.len();
        if offset >= len || limit == 0 {
            return Vec::new(&env);
        }
        let end = (offset + limit).min(len);
        let mut page = Vec::new(&env);
        for i in offset..end {
            page.push_back(all.get(i).unwrap());
        }
        page
    }

    /// Returns a page of asset IDs for the given type together with the total count.
    /// Designed for large fleets where returning the full list would exceed Soroban's
    /// return data limits.
    ///
    /// # Arguments
    /// * `asset_type` - The asset type symbol to query
    /// * `page` - Zero-based page index
    /// * `page_size` - Maximum number of asset IDs per page (capped at 100)
    ///
    /// # Returns
    /// `AssetTypePage` containing the requested slice and the total asset count
    pub fn get_assets_by_type_paginated(
        env: Env,
        asset_type: Symbol,
        page: u32,
        page_size: u32,
    ) -> AssetTypePage {
        const MAX_PAGE_SIZE: u32 = 100;
        let page_size = page_size.min(MAX_PAGE_SIZE);

        let key = type_assets_key(&asset_type);
        let all: Vec<u64> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));
        if env.storage().persistent().has(&key) {
            env.storage().persistent().extend_ttl(&key, 518400, 518400);
        }

        let total = all.len();

        if page_size == 0 {
            return AssetTypePage { assets: Vec::new(&env), total };
        }

        let offset = match page.checked_mul(page_size) {
            Some(o) => o,
            None => return AssetTypePage { assets: Vec::new(&env), total },
        };

        if offset >= total {
            return AssetTypePage { assets: Vec::new(&env), total };
        }

        let end = (offset + page_size).min(total);
        let mut assets = Vec::new(&env);
        for i in offset..end {
            assets.push_back(all.get(i).unwrap());
        }

        AssetTypePage { assets, total }
    }

    /// Initialize the admin address for the contract.
    /// This function should be called once immediately after deployment.
    ///
    /// # Arguments
    /// * `deployer` - The address of the contract deployer; must sign this transaction.
    /// * `admin` - The address that will have administrative privileges
    ///
    /// # Panics
    /// - [`ContractError::AdminAlreadyInitialized`] if admin has already been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if deployer is not the transaction invoker
    pub fn initialize_admin(env: Env, deployer: Address, admin: Address) {
        if deployer != env.invoker() {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        deployer.require_auth();
        if env.storage().instance().has(&ADMIN_KEY) {
            panic_with_error!(&env, ContractError::AdminAlreadyInitialized);
        }
        env.storage().instance().set(&ADMIN_KEY, &admin);
        env.storage()
            .instance()
            .extend_ttl(TTL_THRESHOLD, TTL_TARGET);
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("INIT_ADM")),
            (admin, env.ledger().timestamp()),
        );
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
            .get(&ADMIN_KEY)
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
        if env.storage().instance().has(&PENDING_ADMIN_KEY) {
            panic_with_error!(&env, ContractError::PendingAdminAlreadyExists);
        }
        env.storage().instance().set(&PENDING_ADMIN_KEY, &new_admin);
        env.storage().instance().extend_ttl(518400, 518400);
        env.events().publish(
            (symbol_short!("PROP_ADM"),),
            (admin.clone(), new_admin.clone()),
        );
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("PROP_ADM")),
            (admin, env.ledger().timestamp(), new_admin),
        );
    }

    /// Accept the admin transfer (step 2 of 2-step transfer).
    /// Only the pending admin can accept and become the new admin.
    ///
    /// # Arguments
    /// * `new_admin` - The pending admin address
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if no pending admin exists
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the pending admin
    pub fn accept_admin(env: Env, new_admin: Address) {
        new_admin.require_auth();
        let pending_admin: Address = env
            .storage()
            .instance()
            .get(&PENDING_ADMIN_KEY)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if pending_admin != new_admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        env.storage().instance().set(&ADMIN_KEY, &pending_admin);
        env.storage().instance().remove(&PENDING_ADMIN_KEY);
        env.storage().instance().extend_ttl(518400, 518400);
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("ADMIN_SET")),
            (pending_admin.clone(), env.ledger().timestamp()),
        );
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

    /// Admin-only function to deregister (remove) an asset from the registry.
    /// This permanently removes the asset and all associated data.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset to deregister
    ///
    /// # Behavior
    /// If the dedup key has already expired from storage, the remove operation
    /// is a no-op. This allows the same owner to re-register the same metadata
    /// after the dedup key has naturally expired.
    ///
    /// # Lifecycle Data
    /// Maintenance history, collateral score, score history, and last-update timestamp
    /// stored in the lifecycle contract are **not** removed by this call. They remain
    /// readable by anyone who knows the asset ID and continue to consume storage until
    /// they expire or are explicitly removed. After deregistering, call
    /// `lifecycle::purge_asset_data(admin, asset_id)` to reclaim that storage.
    ///
    /// # Panics
    /// - [`ContractError::AssetNotFound`] if no asset exists with the given ID
    /// - [`ContractError::UnauthorizedOwner`] if caller is neither the admin nor the asset owner
    pub fn deregister_asset(env: Env, caller: Address, asset_id: u64) {
        ensure_not_paused(&env);

        let asset: Asset = env
            .storage()
            .persistent()
            .get(&asset_key(asset_id))
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::AssetNotFound));

        let admin = Self::get_admin(env.clone());
        if caller == admin {
            admin.require_auth();
        } else if caller == asset.owner {
            asset.owner.require_auth();
        } else {
            panic_with_error!(&env, ContractError::UnauthorizedOwner);
        }

        // Remove asset storage
        env.storage().persistent().remove(&asset_key(asset_id));

        // Remove deduplication key
        let dk = dedup_key(
            &asset.owner,
            &asset.asset_type,
            &env.crypto().sha256(&asset.metadata.to_xdr(&env)).into(),
        );
        env.storage().persistent().remove(&dk);

        // Remove from owner index
        owner_index_remove(&env, &asset.owner, asset_id);

        // Decrement type count
        type_count_dec(&env, &asset.asset_type);

        // Remove from type-to-assets index
        type_assets_remove(&env, &asset.asset_type, asset_id);

        // Emit deregistration event
        env.events().publish(
            (DEREG_TOPIC, asset_id),
            (asset.asset_type.clone(), asset.owner.clone()),
        );
    }

    /// Owner-only function to update the metadata of an existing asset.
    /// This is typically used after refurbishment or specification changes.
    /// Removes the old deduplication key and registers a new one.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset to update
    /// * `owner` - The current owner of the asset (must match stored owner)
    /// * `new_metadata` - The new metadata string to assign to the asset
    ///
    /// # Panics
    /// - [`ContractError::AssetNotFound`] if no asset exists with the given ID
    /// - [`ContractError::UnauthorizedOwner`] if caller is not the asset owner
    /// - [`ContractError::DuplicateAsset`] if new metadata already exists for this owner
    pub fn update_asset_metadata(env: Env, asset_id: u64, owner: Address, new_metadata: String) {
        ensure_not_paused(&env);
        owner.require_auth();
        require_string_length(&new_metadata, "metadata", 256);

        let mut asset: Asset = env
            .storage()
            .persistent()
            .get(&asset_key(asset_id))
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::AssetNotFound));

        if asset.owner != owner {
            panic_with_error!(&env, ContractError::UnauthorizedOwner);
        }

        if new_metadata == asset.metadata {
            return;
        }

        // Remove old dedup key
        let old_hash: BytesN<32> = env.crypto().sha256(&asset.metadata.to_xdr(&env)).into();
        env.storage()
            .persistent()
            .remove(&dedup_key(&owner, &asset.asset_type, &old_hash));

        // Reject if new metadata is a duplicate for this owner
        let new_hash: BytesN<32> = env
            .crypto()
            .sha256(&new_metadata.clone().to_xdr(&env))
            .into();
        let new_dk = dedup_key(&owner, &asset.asset_type, &new_hash);
        if env.storage().persistent().has(&new_dk) {
            panic_with_error!(&env, ContractError::DuplicateAsset);
        }

        // Store new dedup key and updated asset
        env.storage().persistent().set(&new_dk, &asset_id);
        env.storage()
            .persistent()
            .extend_ttl(&new_dk, TTL_THRESHOLD, TTL_TARGET);
        asset.metadata = new_metadata.clone();
        asset.metadata_updated_at = env.ledger().timestamp();
        env.storage().persistent().set(&asset_key(asset_id), &asset);
        env.storage()
            .persistent()
            .extend_ttl(&asset_key(asset_id), TTL_THRESHOLD, TTL_TARGET);

        env.events().publish(
            (symbol_short!("UPD_META"), asset_id),
            (owner, new_metadata, env.ledger().timestamp()),
        );
    }

    /// Transfer ownership of an asset from the current owner to a new owner.
    /// Only the current owner can initiate the transfer.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset to transfer
    /// * `current_owner` - The current owner of the asset (must match stored owner)
    /// * `new_owner` - The address of the new asset owner
    ///
    /// # Panics
    /// - [`ContractError::AssetNotFound`] if no asset exists with the given ID
    /// - [`ContractError::UnauthorizedOwner`] if caller is not the current owner
    pub fn transfer_asset(env: Env, asset_id: u64, current_owner: Address, new_owner: Address) {
        ensure_not_paused(&env);
        current_owner.require_auth();

        let mut asset: Asset = env
            .storage()
            .persistent()
            .get(&asset_key(asset_id))
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::AssetNotFound));

        if asset.owner != current_owner {
            panic_with_error!(&env, ContractError::UnauthorizedOwner);
        }

        if current_owner == new_owner {
            panic_with_error!(&env, ContractError::SameOwner);
        }

        // Move dedup key to new owner
        let hash: BytesN<32> = env
            .crypto()
            .sha256(&asset.metadata.clone().to_xdr(&env))
            .into();
        env.storage()
            .persistent()
            .remove(&dedup_key(&current_owner, &asset.asset_type, &hash));
        env.storage()
            .persistent()
            .set(&dedup_key(&new_owner, &asset.asset_type, &hash), &asset_id);
        env.storage().persistent().extend_ttl(
            &dedup_key(&new_owner, &asset.asset_type, &hash),
            TTL_THRESHOLD,
            TTL_TARGET,
        );

        // Move owner index entry
        owner_index_remove(&env, &current_owner, asset_id);
        owner_index_add(&env, &new_owner, asset_id);

        asset.owner = new_owner.clone();
        env.storage().persistent().set(&asset_key(asset_id), &asset);
        env.storage()
            .persistent()
            .extend_ttl(&asset_key(asset_id), TTL_THRESHOLD, TTL_TARGET);

        env.events().publish(
            (symbol_short!("TRANSFER"), asset_id),
            (current_owner, new_owner, env.ledger().timestamp()),
        );
    }

    /// Admin-only function to decommission an asset.
    /// Sets the decommissioned flag and resets the collateral score to 0.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored admin
    /// * `asset_id` - The unique identifier of the asset to decommission
    ///
    /// # Panics
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    /// - [`ContractError::AssetNotFound`] if no asset exists with the given ID
    pub fn decommission_asset(env: Env, admin: Address, asset_id: u64) {
        ensure_not_paused(&env);
        admin.require_auth();

        let stored_admin: Address = Self::get_admin(env.clone());
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        // Verify asset exists
        if !Self::asset_exists(env.clone(), asset_id) {
            panic_with_error!(&env, ContractError::AssetNotFound);
        }

        // Set decommissioned flag
        let decomm_key = decommissioned_key(asset_id);
        env.storage().persistent().set(&decomm_key, &true);
        env.storage().persistent().extend_ttl(&decomm_key, TTL_THRESHOLD, TTL_TARGET);

        // Clear the under_maintenance flag when decommissioning
        let maint_key = (symbol_short!("U_MAINT"), asset_id);
        env.storage().persistent().remove(&maint_key);

        // Emit decommission event with asset_id and ledger sequence
        let ledger_seq = env.ledger().sequence();
        env.events()
            .publish((symbol_short!("DECOMM"), asset_id), ledger_seq);
    }

    /// Propose a WASM upgrade for the asset registry contract.
    /// Must be followed by `execute_upgrade` after the timelock delay.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored admin
    /// * `new_wasm_hash` - The hash of the new WASM to deploy
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if the admin has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    pub fn propose_upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) {
        ensure_not_paused(&env);
        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        env.storage().instance().extend_ttl(518400, 518400);

        let tl_key = global_timelock_key(symbol_short!("UPGRADE"));
        env.storage().persistent().set(
            &tl_key,
            &TimelockProposal {
                proposed_at: env.ledger().timestamp(),
                executed: false,
            },
        );
        env.storage()
            .persistent()
            .extend_ttl(&tl_key, TTL_THRESHOLD, TTL_TARGET);
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
    /// * `admin` - The admin address that must match the stored admin
    ///
    /// # Panics
    /// - [`ContractError::NotInitialized`] if the admin has not been initialized
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    /// - [`ContractError::ProposalNotFound`] if no upgrade was proposed or already executed
    /// - [`ContractError::TimelockNotExpired`] if the delay has not elapsed
    pub fn execute_upgrade(env: Env, admin: Address) {
        ensure_not_paused(&env);
        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::NotInitialized));
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        require_global_timelock_ready(&env, symbol_short!("UPGRADE"));

        let new_wasm_hash: BytesN<32> = env
            .storage()
            .persistent()
            .get(&symbol_short!("PEND_UPG"))
            .unwrap_or_else(|| panic_with_error!(&env, ContractError::ProposalNotFound));
        env.storage()
            .persistent()
            .remove(&symbol_short!("PEND_UPG"));

        env.storage().instance().extend_ttl(518400, 518400);

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
            env.deployer()
                .update_current_contract_wasm(new_wasm_hash);
        }
    }

    /// Admin-only function to allow a new asset type symbol.
    ///
    /// # Arguments
    /// * `admin` - The address that must match the stored admin
    /// * `asset_type` - The symbol of the new asset type to allow
    pub fn add_asset_type(env: Env, admin: Address, asset_type: Symbol) {
        admin.require_auth();
        let stored_admin: Address = Self::get_admin(env.clone());
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        env.storage()
            .persistent()
            .set(&asset_type_key(&asset_type), &true);
        env.storage().persistent().extend_ttl(
            &asset_type_key(&asset_type),
            TTL_THRESHOLD,
            TTL_TARGET,
        );
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("ADD_TYPE")),
            (admin, env.ledger().timestamp(), asset_type.clone()),
        );
        env.events().publish((ADD_TYPE_TOPIC,), (asset_type,));
    }

    /// Admin-only function to remove an asset type from the allowlist.
    /// Removal is blocked if any registered assets of this type still exist.
    ///
    /// # Arguments
    /// * `admin` - The address that must match the stored admin
    /// * `asset_type` - The symbol of the asset type to remove
    ///
    /// # Panics
    /// - [`ContractError::TypeInUse`] if one or more assets of this type are still registered
    pub fn remove_asset_type(env: Env, admin: Address, asset_type: Symbol) {
        admin.require_auth();
        let stored_admin: Address = Self::get_admin(env.clone());
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }
        let count: u64 = env
            .storage()
            .persistent()
            .get(&type_count_key(&asset_type))
            .unwrap_or(0);
        if count > 0 {
            panic_with_error!(&env, ContractError::TypeInUse);
        }
        env.storage()
            .persistent()
            .remove(&asset_type_key(&asset_type));
        env.events().publish(
            (symbol_short!("ADM_AUD"), symbol_short!("RM_TYPE")),
            (admin, env.ledger().timestamp(), asset_type.clone()),
        );
        env.events().publish((RM_TYPE_TOPIC,), (asset_type,));
    }

    /// Check if an asset type is valid (exists in the allowlist).
    ///
    /// # Arguments
    /// * `asset_type` - The symbol of the asset type to check
    ///
    /// # Returns
    /// `true` if valid; `false` otherwise
    pub fn is_valid_asset_type(env: Env, asset_type: Symbol) -> bool {
        env.storage()
            .persistent()
            .get(&asset_type_key(&asset_type))
            .unwrap_or(false)
    }

    /// Get the lifecycle score for an asset by cross-calling the Lifecycle contract.
    ///
    /// # Arguments
    /// * `asset_id` - The unique identifier of the asset
    /// * `lifecycle_contract` - The address of the Lifecycle contract
    ///
    /// # Returns
    /// The collateral score (u32) for the asset
    ///
    /// # Panics
    /// - [`ContractError::AssetNotFound`] if the asset does not exist
    pub fn get_lifecycle_score(env: Env, asset_id: u64, lifecycle_contract: Address) -> u32 {
        // Verify asset exists in this registry
        if !Self::asset_exists(env.clone(), asset_id) {
            panic_with_error!(&env, ContractError::AssetNotFound);
        }

        // Cross-call the Lifecycle contract to get the collateral score
        // Using invoke_contract to avoid circular dependency
        let args = soroban_sdk::vec![
            &env,
            soroban_sdk::IntoVal::<Env, soroban_sdk::Val>::into_val(&asset_id, &env)
        ];
        let score: u32 = env.invoke_contract(
            &lifecycle_contract,
            &Symbol::new(&env, "get_collateral_score"),
            args,
        );
        score
    }

    /// Decommission an asset and notify the lifecycle contract to freeze the score.
    ///
    /// This combines the registry-side decommission flag with a cross-contract call
    /// to the lifecycle contract so the collateral score is captured at decommission
    /// time and no longer decays. Lenders will see the final verified state.
    ///
    /// # Arguments
    /// * `admin` - The admin address that must match the stored admin
    /// * `asset_id` - The unique identifier of the asset to decommission
    /// * `lifecycle_contract` - Address of the lifecycle contract to notify
    ///
    /// # Panics
    /// - [`ContractError::UnauthorizedAdmin`] if caller is not the admin
    /// - [`ContractError::AssetNotFound`] if no asset exists with the given ID
    pub fn decommission_asset_notify(
        env: Env,
        admin: Address,
        asset_id: u64,
        lifecycle_contract: Address,
    ) {
        ensure_not_paused(&env);
        admin.require_auth();

        let stored_admin: Address = Self::get_admin(env.clone());
        if stored_admin != admin {
            panic_with_error!(&env, ContractError::UnauthorizedAdmin);
        }

        if !Self::asset_exists(env.clone(), asset_id) {
            panic_with_error!(&env, ContractError::AssetNotFound);
        }

        let decomm_key = decommissioned_key(asset_id);
        env.storage().persistent().set(&decomm_key, &true);
        env.storage()
            .persistent()
            .extend_ttl(&decomm_key, TTL_THRESHOLD, TTL_TARGET);

        let maint_key = (symbol_short!("U_MAINT"), asset_id);
        env.storage().persistent().remove(&maint_key);

        let ledger_seq = env.ledger().sequence();
        env.events()
            .publish((symbol_short!("DECOMM"), asset_id), ledger_seq);

        // Notify lifecycle to freeze the collateral score at its current value.
        let args = soroban_sdk::vec![
            &env,
            soroban_sdk::IntoVal::<Env, soroban_sdk::Val>::into_val(&asset_id, &env)
        ];
        env.invoke_contract::<()>(
            &lifecycle_contract,
            &Symbol::new(&env, "decommission_notify"),
            args,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::storage::Instance as _;
    use soroban_sdk::testutils::storage::Persistent;
    use soroban_sdk::{
        symbol_short,
        testutils::{Address as _, Events, Ledger as _, Logs},
        Bytes, Env, FromVal, String, Symbol, TryIntoVal,
    };

    use crate::AssetRegistryClient;
    use engineer_registry;
    use lifecycle;

    fn unique_serial(env: &Env) -> String {
        use core::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
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

    /// Wrapper: register_asset with an auto-generated unique serial number.
    fn reg(client: &AssetRegistryClient, env: &Env, asset_type: Symbol, metadata: String, owner: &Address) -> u64 {
        client.register_asset(&asset_type, &metadata, &unique_serial(env), owner)
    }

    /// Wrapper: try_register_asset with an auto-generated unique serial number.
    #[allow(dead_code)]
    fn try_reg(client: &AssetRegistryClient, env: &Env, asset_type: Symbol, metadata: String, owner: &Address) -> Result<Result<u64, soroban_sdk::Error>, Result<soroban_sdk::Error, soroban_sdk::InvokeError>> {
        client.try_register_asset(&asset_type, &metadata, &unique_serial(env), owner)
    }

    #[test]
    fn test_register_and_get_asset() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Caterpillar 3516 Generator"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(id, 1);

        let asset = client.get_asset(&id);
        assert_eq!(asset.asset_id, 1);
        assert_eq!(asset.owner, owner);
    }

    #[test]
    fn test_get_asset_returns_correct_owner() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("TURBINE"));

        let expected_owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("TURBINE"),
            &String::from_str(&env, "GE LM2500 Turbine"),
            &unique_serial(&env),
            &expected_owner,
        );

        let asset = client.get_asset(&id);
        assert_eq!(asset.owner, expected_owner);
    }

    #[test]
    fn test_get_asset_not_found() {
        let env = Env::default();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);
        let result = client.try_get_asset(&999);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32
            )))
        );
    }

    #[test]
    fn test_duplicate_metadata_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let metadata = String::from_str(&env, "CAT-3516-SN123456");
        let serial = String::from_str(&env, "SN-CAT-3516-001");

        // First registration succeeds
        let id = client.register_asset(&symbol_short!("GENSET"), &metadata, &serial, &owner);
        assert_eq!(id, 1);

        // Second registration with same serial is rejected (same physical machine)
        let result = client.try_register_asset(&symbol_short!("GENSET"), &metadata, &serial, &owner);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::DuplicateAsset as u32
            )))
        );
    }

    #[test]
    fn test_register_asset_duplicate_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let metadata = String::from_str(&env, "CAT-3516-DUPLICATE");
        let serial = String::from_str(&env, "SN-CAT-3516-DUP");

        let id = client.register_asset(&symbol_short!("GENSET"), &metadata, &serial, &owner);
        assert_eq!(id, 1);

        let result = client.try_register_asset(&symbol_short!("GENSET"), &metadata, &serial, &owner);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::DuplicateAsset as u32
            )))
        );
    }

    #[test]
    fn test_different_owners_same_metadata_allowed() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner_a = Address::generate(&env);
        let owner_b = Address::generate(&env);
        let metadata = String::from_str(&env, "CAT-3516-SN123456");

        // Different owners may register the same metadata (different physical assets)
        let id_a = client.register_asset(&symbol_short!("GENSET"), &metadata, &String::from_str(&env, "SN-A-001"), &owner_a);
        let id_b = client.register_asset(&symbol_short!("GENSET"), &metadata, &String::from_str(&env, "SN-B-001"), &owner_b);
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn test_register_asset_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let asset_type = symbol_short!("GENSET");
        let metadata = String::from_str(&env, "Caterpillar 3516 Generator");

        let timestamp = env.ledger().timestamp();
        let asset_id = client.register_asset(&asset_type, &metadata, &unique_serial(&env), &owner);

        use soroban_sdk::TryIntoVal;
        let reg_topic = symbol_short!("reg_asset");
        let events = env.events().all();
        let (_, topics, data) = events.last().unwrap();

        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, reg_topic);

        let (emitted_id, emitted_owner, emitted_timestamp): (u64, Address, u64) =
            data.try_into_val(&env).unwrap();
        assert_eq!(emitted_id, asset_id);
        assert_eq!(emitted_owner, owner);
        assert_eq!(emitted_timestamp, timestamp);
    }

    #[test]
    fn test_ttl_extended_on_registration() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let asset_type = symbol_short!("GENSET");
        let metadata = String::from_str(&env, "Caterpillar 3516 Generator");

        let id = client.register_asset(&asset_type, &metadata, &unique_serial(&env), &owner);

        // Verify TTL is set for asset storage entry
        let asset_ttl = env.as_contract(&contract_id, || {
            env.storage().persistent().get_ttl(&asset_key(id))
        });
        assert!(asset_ttl > 0, "Asset TTL should be extended");

        // Verify TTL is set for deduplication key
        let meta_bytes = metadata.to_xdr(&env);
        let meta_hash: BytesN<32> = env.crypto().sha256(&meta_bytes).into();
        let dedup_ttl = env.as_contract(&contract_id, || {
            let dk = dedup_key(&owner, &symbol_short!("GENSET"), &meta_hash);
            env.storage().persistent().get_ttl(&dk)
        });
        assert!(dedup_ttl > 0, "Deduplication key TTL should be extended");
    }

    #[test]
    fn test_register_asset_dedup_key_ttl_is_set() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let metadata = String::from_str(&env, "Dedup TTL test asset");
        client.register_asset(&symbol_short!("GENSET"), &metadata, &unique_serial(&env), &owner);

        let meta_bytes = metadata.to_xdr(&env);
        let meta_hash: BytesN<32> = env.crypto().sha256(&meta_bytes).into();
        let ttl = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get_ttl(&dedup_key(&owner, &symbol_short!("GENSET"), &meta_hash))
        });
        assert!(
            ttl > 0,
            "dedup key TTL must be extended after register_asset"
        );
    }

    #[test]
    fn test_admin_can_propose_upgrade() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        let new_wasm_hash = BytesN::from_array(&env, &[0xabu8; 32]);
        let result = client.try_propose_upgrade(&admin, &new_wasm_hash);
        assert_ne!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_non_admin_cannot_propose_upgrade() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

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
    fn test_propose_and_accept_admin_transfer() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        client.propose_admin(&admin, &new_admin);
        client.accept_admin(&new_admin);

        assert_eq!(client.get_admin(), new_admin);
    }

    #[test]
    fn test_accept_admin_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        client.propose_admin(&admin, &new_admin);
        client.accept_admin(&new_admin);

        // propose_admin emits PROP_ADM; accept_admin must emit ADMIN_SET
        let events = env.events().all();
        let (_, topics, data) = events.last().unwrap();
        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("ADMIN_SET"));

        let (emitted_admin,): (Address,) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, new_admin);
    }

    #[test]
    fn test_pending_admin_key_cleared_after_accept() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        client.propose_admin(&admin, &new_admin);
        client.accept_admin(&new_admin);

        env.as_contract(&contract_id, || {
            assert!(!env.storage().instance().has(&PENDING_ADMIN_KEY));
        });
    }

    #[test]
    fn test_non_admin_cannot_propose_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let outsider = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        let result = client.try_propose_admin(&outsider, &new_admin);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32,
            ))),
        );
    }

    #[test]
    fn test_propose_admin_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        client.propose_admin(&admin, &new_admin);

        let events = env.events().all();
        assert!(events.len() >= 1);
        let (_, topics, data): (_, soroban_sdk::Vec<soroban_sdk::Val>, soroban_sdk::Val) =
            events.get(0).unwrap();
        assert_eq!(
            Symbol::from_val(&env, &topics.get(0).unwrap()),
            symbol_short!("PROP_ADM")
        );
        let (emitted_admin, emitted_new_admin): (Address, Address) =
            soroban_sdk::FromVal::from_val(&env, &data);
        assert_eq!(emitted_admin, admin);
        assert_eq!(emitted_new_admin, new_admin);
    }

    #[test]
    fn test_wrong_address_cannot_accept_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let impostor = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.propose_admin(&admin, &new_admin);

        use soroban_sdk::IntoVal;
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &impostor,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "accept_admin",
                args: (&impostor,).into_val(&env),
                sub_invokes: &[],
            },
        }]);

        let result = client.try_accept_admin(&impostor);
        assert!(result.is_err());
        // Original admin unchanged
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    fn test_owner_can_update_metadata() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Original spec"),
            &unique_serial(&env),
            &owner,
        );

        client.update_asset_metadata(&id, &owner, &String::from_str(&env, "Refurbished spec v2"));

        let asset = client.get_asset(&id);
        assert_eq!(
            asset.metadata,
            String::from_str(&env, "Refurbished spec v2")
        );
    }

    #[test]
    fn test_update_metadata_stamps_metadata_updated_at() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Original spec"),
            &unique_serial(&env),
            &owner,
        );

        // Advance ledger time before updating
        env.ledger().with_mut(|li| li.timestamp += 1000);
        let update_time = env.ledger().timestamp();

        client.update_asset_metadata(&id, &owner, &String::from_str(&env, "Refurbished spec v2"));

        let asset = client.get_asset(&id);
        assert_eq!(asset.metadata_updated_at, update_time);
        assert!(asset.metadata_updated_at > asset.registered_at);
    }

    #[test]
    fn test_update_metadata_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Original spec"),
            &unique_serial(&env),
            &owner,
        );

        client.update_asset_metadata(&id, &owner, &String::from_str(&env, "Refurbished spec v2"));

        // env.events().all() reflects only the most recent contract call
        assert_eq!(env.events().all().len(), 1);
    }

    #[test]
    fn test_update_metadata_skips_noop() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Original spec"),
            &unique_serial(&env),
            &owner,
        );

        let original_asset = client.get_asset(&id);
        client.update_asset_metadata(&id, &owner, &String::from_str(&env, "Original spec"));

        let updated_asset = client.get_asset(&id);
        assert_eq!(updated_asset.metadata, original_asset.metadata);
        assert_eq!(
            updated_asset.metadata_updated_at,
            original_asset.metadata_updated_at
        );
        assert_eq!(env.events().all().len(), 0);
    }

    #[test]
    fn test_non_owner_cannot_update_metadata() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Original spec"),
            &unique_serial(&env),
            &owner,
        );

        let attacker = Address::generate(&env);
        let result = client.try_update_asset_metadata(
            &id,
            &attacker,
            &String::from_str(&env, "Hacked spec"),
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedOwner as u32,
            ))),
        );
    }

    #[test]
    fn test_update_metadata_nonexistent_asset() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let owner = Address::generate(&env);
        let result =
            client.try_update_asset_metadata(&999u64, &owner, &String::from_str(&env, "New spec"));
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_transfer_asset() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let new_owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        client.transfer_asset(&id, &owner, &new_owner);

        let asset = client.get_asset(&id);
        assert_eq!(asset.owner, new_owner);
    }

    #[test]
    fn test_transfer_asset_same_owner_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        let result = client.try_transfer_asset(&id, &owner, &owner);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::SameOwner as u32
            )))
        );
        // Asset still belongs to original owner
        assert_eq!(client.get_asset(&id).owner, owner);
    }

    #[test]
    fn test_transfer_asset_non_owner_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let attacker = Address::generate(&env);
        let new_owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        let result = client.try_transfer_asset(&id, &attacker, &new_owner);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedOwner as u32,
            ))),
        );
    }

    #[test]
    fn test_transfer_asset_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let new_owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        client.transfer_asset(&id, &owner, &new_owner);

        // env.events().all() reflects only the most recent contract call
        assert_eq!(env.events().all().len(), 1);
    }

    #[test]
    fn test_transfer_updates_dedup_so_new_owner_can_register_same_metadata() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let new_owner = Address::generate(&env);
        let metadata = String::from_str(&env, "CAT-3516");

        let id = client.register_asset(&symbol_short!("GENSET"), &metadata, &unique_serial(&env), &owner);
        client.transfer_asset(&id, &owner, &new_owner);

        // Original owner can now register the same metadata again (dedup key was moved)
        let id2 = client.register_asset(&symbol_short!("GENSET"), &metadata, &unique_serial(&env), &owner);
        assert_ne!(id, id2);
    }

    #[test]
    fn test_update_metadata_dedup_enforced() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        // Register two assets with different metadata
        let id1 = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Spec A"),
            &unique_serial(&env),
            &owner,
        );
        client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Spec B"),
            &unique_serial(&env),
            &owner,
        );

        // Trying to update asset 1 to "Spec B" (already taken by same owner) should fail
        let result =
            client.try_update_asset_metadata(&id1, &owner, &String::from_str(&env, "Spec B"));
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::DuplicateAsset as u32,
            ))),
        );
    }

    #[test]
    fn test_asset_exists_returns_true_for_existing_asset() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Turbine X"),
            &unique_serial(&env),
            &owner,
        );

        assert!(client.asset_exists(&id));
    }

    #[test]
    fn test_asset_exists_returns_false_for_nonexistent_asset() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        assert!(!client.asset_exists(&9999u64));
    }

    #[test]
    fn test_get_assets_by_owner_returns_registered_ids() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));
        client.add_asset_type(&admin, &symbol_short!("TURBINE"));

        let owner = Address::generate(&env);
        let id1 = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Asset Alpha"),
            &unique_serial(&env),
            &owner,
        );
        let id2 = client.register_asset(
            &symbol_short!("TURBINE"),
            &String::from_str(&env, "Asset Beta"),
            &unique_serial(&env),
            &owner,
        );

        let ids = client.get_assets_by_owner(&owner);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }

    #[test]
    fn test_get_assets_by_owner_returns_empty_for_unknown_owner() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let stranger = Address::generate(&env);
        let ids = client.get_assets_by_owner(&stranger);
        assert_eq!(ids.len(), 0);
    }

    #[test]
    fn test_get_assets_by_owner_page_returns_paged_ids() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let mut ids: Vec<u64> = Vec::new(&env);
        ids.push_back(client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Asset 0"),
            &unique_serial(&env),
            &owner,
        ));
        ids.push_back(client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Asset 1"),
            &unique_serial(&env),
            &owner,
        ));
        ids.push_back(client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Asset 2"),
            &unique_serial(&env),
            &owner,
        ));
        ids.push_back(client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Asset 3"),
            &unique_serial(&env),
            &owner,
        ));
        ids.push_back(client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Asset 4"),
            &unique_serial(&env),
            &owner,
        ));

        let page0 = client.get_assets_by_owner_page(&owner, &0, &2);
        assert_eq!(page0.len(), 2);
        assert_eq!(page0.get(0).unwrap(), ids.get(0).unwrap());
        assert_eq!(page0.get(1).unwrap(), ids.get(1).unwrap());

        let page1 = client.get_assets_by_owner_page(&owner, &1, &2);
        assert_eq!(page1.len(), 2);
        assert_eq!(page1.get(0).unwrap(), ids.get(2).unwrap());
        assert_eq!(page1.get(1).unwrap(), ids.get(3).unwrap());

        let page2 = client.get_assets_by_owner_page(&owner, &2, &2);
        assert_eq!(page2.len(), 1);
        assert_eq!(page2.get(0).unwrap(), ids.get(4).unwrap());

        let page3 = client.get_assets_by_owner_page(&owner, &3, &2);
        assert_eq!(page3.len(), 0);
    }

    #[test]
    fn test_get_assets_by_owner_page_returns_empty_for_unknown_owner_or_zero_page_size() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let owner = Address::generate(&env);
        let unknown_owner = Address::generate(&env);
        client.register_asset(&symbol_short!("GENSET"), &String::from_str(&env, "Asset X"), &unique_serial(&env), &owner);

        assert_eq!(client.get_assets_by_owner_page(&unknown_owner, &0, &2).len(), 0);
        assert_eq!(client.get_assets_by_owner_page(&owner, &0, &0).len(), 0);
        assert_eq!(client.get_assets_by_owner_page(&owner, &5, &2).len(), 0);
    }

    #[test]
    fn test_get_assets_by_owner_updated_after_transfer() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let new_owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        client.transfer_asset(&id, &owner, &new_owner);

        // Original owner should have no assets
        assert_eq!(client.get_assets_by_owner(&owner).len(), 0);
        // New owner should have the asset
        let new_ids = client.get_assets_by_owner(&new_owner);
        assert_eq!(new_ids.len(), 1);
        assert!(new_ids.contains(&id));
    }

    #[test]
    fn test_transfer_asset_logs_missing_owner_index_and_keeps_old_owner_clean() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let new_owner = Address::generate(&env);
        let retained_id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );
        let transferred_id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3520"),
            &unique_serial(&env),
            &owner,
        );

        env.as_contract(&contract_id, || {
            env.storage().persistent().remove(&owner_index_key(&owner));
        });

        client.transfer_asset(&transferred_id, &owner, &new_owner);

        let logs = env.logs().all();
        let warning = logs.last().unwrap();
        assert!(warning.contains("owner index missing during remove"));

        let old_owner_ids = client.get_assets_by_owner(&owner);
        assert_eq!(old_owner_ids.len(), 0);
        assert!(!old_owner_ids.contains(&transferred_id));
        assert!(!old_owner_ids.contains(&retained_id));

        let new_owner_ids = client.get_assets_by_owner(&new_owner);
        assert_eq!(new_owner_ids.len(), 1);
        assert!(new_owner_ids.contains(&transferred_id));
    }

    #[test]
    fn test_owner_index_remove_missing_key_emits_diagnostic_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        // Remove the owner index to simulate expiry
        env.as_contract(&contract_id, || {
            env.storage().persistent().remove(&owner_index_key(&owner));
        });

        // Trigger owner_index_remove via deregister
        client.deregister_asset(&admin, &id);

        // Verify the IDX_MISS diagnostic event was emitted
        let events = env.events().all();
        let idx_miss_event = events.iter().find(|(_, topics, _)| {
            use soroban_sdk::TryIntoVal;
            topics
                .get(0)
                .and_then(|v| {
                    let s: Result<Symbol, _> = v.try_into_val(&env);
                    s.ok()
                })
                .map(|s| s == symbol_short!("IDX_MISS"))
                .unwrap_or(false)
        });
        assert!(
            idx_miss_event.is_some(),
            "IDX_MISS diagnostic event must be emitted when owner index is missing"
        );
    }

    #[test]
    fn test_owner_index_key_removed_after_last_asset_deregistered() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        client.deregister_asset(&admin, &id);

        let key_exists = env.as_contract(&contract_id, || {
            env.storage().persistent().has(&owner_index_key(&owner))
        });
        assert!(
            !key_exists,
            "owner index key must be absent after last asset is removed"
        );
    }

    #[test]
    fn test_update_asset_metadata_removes_old_dedup_key() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let meta_a = String::from_str(&env, "Spec A");
        let meta_b = String::from_str(&env, "Spec B");

        // Register with metadata A, then update to B
        let id = client.register_asset(&symbol_short!("GENSET"), &meta_a, &unique_serial(&env), &owner);
        client.update_asset_metadata(&id, &owner, &meta_b);

        // Old dedup key (A) is gone — owner can register metadata A again
        let id2 = client.register_asset(&symbol_short!("GENSET"), &meta_a, &unique_serial(&env), &owner);
        assert_ne!(id, id2);

        // New dedup key (B) is present — owner cannot register metadata B again
        let result = client.try_register_asset(&symbol_short!("GENSET"), &meta_b, &unique_serial(&env), &owner);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::DuplicateAsset as u32,
            ))),
        );
    }

    #[test]
    #[should_panic]
    fn test_initialize_admin_called_twice_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        // Second call must panic
        client.initialize_admin(&admin, &admin);
    }

    #[test]
    fn test_get_assets_by_owner_updated_after_deregister() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        assert_eq!(client.get_assets_by_owner(&owner).len(), 1);
        client.deregister_asset(&admin, &id);
        assert_eq!(client.get_assets_by_owner(&owner).len(), 0);
    }

    #[test]
    fn test_deregister_allows_reregistration_of_same_metadata() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let metadata = String::from_str(&env, "CAT-3516");

        // Register asset
        let id1 = client.register_asset(&symbol_short!("GENSET"), &metadata, &unique_serial(&env), &owner);

        // Deregister removes dedup key
        client.deregister_asset(&admin, &id1);

        // Same owner can now re-register the same metadata
        let id2 = client.register_asset(&symbol_short!("GENSET"), &metadata, &unique_serial(&env), &owner);
        assert_ne!(id1, id2);
    }

    // --- Issue #142: get_admin structured error before initialization ---

    #[test]
    fn test_get_admin_before_init_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let result = client.try_get_admin();
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::NotInitialized as u32,
            ))),
        );
    }

    #[test]
    fn test_deregister_asset_with_expired_owner_index() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        // Simulate owner index expiration by removing it
        env.as_contract(&contract_id, || {
            let key = owner_index_key(&owner);
            env.storage().persistent().remove(&key);
        });

        // Deregister should not create a stale empty entry
        client.deregister_asset(&admin, &id);

        // Verify owner index was not recreated
        env.as_contract(&contract_id, || {
            let key = owner_index_key(&owner);
            assert!(!env.storage().persistent().has(&key));
        });
    }

    #[test]
    fn test_transfer_asset_extends_new_owner_dedup_key_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let new_owner = Address::generate(&env);
        let metadata = String::from_str(&env, "CAT-3516");
        let id = client.register_asset(&symbol_short!("GENSET"), &metadata, &unique_serial(&env), &owner);

        client.transfer_asset(&id, &owner, &new_owner);

        // Verify new owner's dedup key TTL is extended
        env.as_contract(&contract_id, || {
            let meta_bytes = Bytes::from(metadata.to_xdr(&env));
            let meta_hash: BytesN<32> = env.crypto().sha256(&meta_bytes).into();
            let new_dk = dedup_key(&new_owner, &symbol_short!("GENSET"), &meta_hash);
            let dedup_ttl = env.storage().persistent().get_ttl(&new_dk);
            assert!(
                dedup_ttl > 0,
                "New owner's dedup key TTL should be extended"
            );
        });
    }

    #[test]
    fn test_update_metadata_extends_new_dedup_key_and_asset_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Original spec"),
            &unique_serial(&env),
            &owner,
        );

        client.update_asset_metadata(&id, &owner, &String::from_str(&env, "Updated spec"));

        // Verify new dedup key TTL is extended
        env.as_contract(&contract_id, || {
            let new_metadata = String::from_str(&env, "Updated spec");
            let meta_bytes = Bytes::from(new_metadata.to_xdr(&env));
            let meta_hash: BytesN<32> = env.crypto().sha256(&meta_bytes).into();
            let new_dk = dedup_key(&owner, &symbol_short!("GENSET"), &meta_hash);
            let dedup_ttl = env.storage().persistent().get_ttl(&new_dk);
            assert!(dedup_ttl > 0, "New dedup key TTL should be extended");

            // Verify asset record TTL is extended
            let asset_ttl = env.storage().persistent().get_ttl(&asset_key(id));
            assert!(asset_ttl > 0, "Asset record TTL should be extended");
        });
    }

    #[test]
    fn test_batch_register_assets_rejects_duplicate_existing_metadata() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "A"),
            &unique_serial(&env),
            &owner,
        );

        let mut batch = Vec::new(&env);
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "A"),
            serial_number: unique_serial(&env),
        });

        let result = client.try_batch_register_assets(&owner, &batch);

        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::DuplicateAsset as u32,
            ))),
        );
    }

    #[test]
    fn test_batch_register_assets_success_and_pause() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let mut batch = Vec::new(&env);
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "A"),
            serial_number: unique_serial(&env),
        });
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "B"),
            serial_number: unique_serial(&env),
        });

        let ids = client.batch_register_assets(&owner, &batch);
        assert_eq!(ids.len(), 2);

        client.pause(&admin);
        let result = client.try_batch_register_assets(&owner, &batch);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32,
            ))),
        );

        client.unpause(&admin);
        let id3 = client.batch_register_assets(&owner, &Vec::new(&env));
        assert_eq!(id3.len(), 0);
    }

    #[test]
    fn test_pause_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.pause(&admin);

        let events = env.events().all();
        assert!(events.len() >= 1);
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
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.pause(&admin);
        client.unpause(&admin);

        let events = env.events().all();
        assert!(events.len() >= 1);
        let (_, topics, data) = events.get(0).unwrap();
        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("UNPAUSED"));
        let (emitted_admin,): (Address,) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, admin);
    }

    #[test]
    fn test_pause_affects_all_state_changes() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Base"),
            &unique_serial(&env),
            &owner,
        );

        client.pause(&admin);

        // Read-only access should still work while paused
        let paused_asset = client.get_asset(&id);
        assert_eq!(paused_asset.asset_id, id);
        assert_eq!(paused_asset.owner, owner);
        assert!(client.asset_exists(&id));
        assert_eq!(client.get_assets_by_owner(&owner).len(), 1);
        assert!(client.try_get_asset(&id).is_ok());

        // register_asset
        assert_eq!(
            client.try_register_asset(
                &symbol_short!("GENSET"),
                &String::from_str(&env, "A"),
                &unique_serial(&env),
                &owner
            ),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // update_asset_metadata
        assert_eq!(
            client.try_update_asset_metadata(&id, &owner, &String::from_str(&env, "New")),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // transfer_asset
        assert_eq!(
            client.try_transfer_asset(&id, &owner, &Address::generate(&env)),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );

        // deregister_asset
        assert_eq!(
            client.try_deregister_asset(&owner, &id),
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
    fn test_batch_register_assets_internal_duplicates_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let mut batch = Vec::new(&env);
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "Duplicate"),
            serial_number: unique_serial(&env),
        });
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "Duplicate"),
            serial_number: unique_serial(&env),
        });

        let result = client.try_batch_register_assets(&owner, &batch);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::DuplicateAsset as u32,
            ))),
        );
    }

    #[test]
    fn test_batch_register_assets_emits_batch_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let mut batch = Vec::new(&env);
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "A"),
            serial_number: unique_serial(&env),
        });
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "B"),
            serial_number: unique_serial(&env),
        });

        let ids = client.batch_register_assets(&owner, &batch);

        // 2 REG_AST + 1 BATCH_REG
        let events = env.events().all();
        assert_eq!(events.len(), 3);

        // Last event must be the BATCH_REG with the correct topic and assigned IDs
        let (_, topics, data) = events.last().unwrap();
        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        let t1: Address = topics.get(1).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("BATCH_REG"));
        assert_eq!(t1, owner);

        let (emitted_ids, _timestamp): (Vec<u64>, u64) = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_ids.len(), 2);
        assert_eq!(emitted_ids.get(0).unwrap(), ids.get(0).unwrap());
        assert_eq!(emitted_ids.get(1).unwrap(), ids.get(1).unwrap());
    }

    #[test]
    fn test_batch_register_assets_empty_emits_no_batch_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        let owner = Address::generate(&env);
        client.batch_register_assets(&owner, &Vec::new(&env));

        // Empty batch — no events at all
        assert_eq!(env.events().all().len(), 0);
    }

    #[test]
    fn test_batch_register_assets_contiguous_ids() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);

        // Register one asset first so ASSET_COUNT starts at 1
        let single = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "first"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(single, 1);

        // Batch of three should get IDs 2, 3, 4 — contiguous, no gaps
        let mut batch = Vec::new(&env);
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "A"),
            serial_number: unique_serial(&env),
        });
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "B"),
            serial_number: unique_serial(&env),
        });
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "C"),
            serial_number: unique_serial(&env),
        });

        let ids = client.batch_register_assets(&owner, &batch);
        assert_eq!(ids.len(), 3);
        assert_eq!(ids.get(0).unwrap(), 2);
        assert_eq!(ids.get(1).unwrap(), 3);
        assert_eq!(ids.get(2).unwrap(), 4);
    }

    #[test]
    fn test_asset_type_allowlist() {
        let env = Env::default();
        env.mock_all_auths();
        let (_contract_id, client, admin) = {
            let contract_id = env.register(AssetRegistry, ());
            let client = AssetRegistryClient::new(&env, &contract_id);
            let admin = Address::generate(&env);
            client.initialize_admin(&admin, &admin);
            (contract_id, client, admin)
        };

        let owner = Address::generate(&env);
        let valid_type = symbol_short!("VALID");
        let invalid_type = symbol_short!("JUNK");

        // Try registering without allowing first
        let result = client.try_register_asset(
            &valid_type,
            &String::from_str(&env, "Some metadata"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidAssetType as u32
            )))
        );

        // Allow the type
        client.add_asset_type(&admin, &valid_type);
        assert!(client.is_valid_asset_type(&valid_type));

        // Now registration succeeds
        let id = client.register_asset(
            &valid_type,
            &String::from_str(&env, "Some metadata"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(id, 1);

        // Still cannot register invalid type
        let result = client.try_register_asset(
            &invalid_type,
            &String::from_str(&env, "Other metadata"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidAssetType as u32
            )))
        );

        // Remove the type — must deregister the asset first
        client.deregister_asset(&owner, &id);
        client.remove_asset_type(&admin, &valid_type);
        assert!(!client.is_valid_asset_type(&valid_type));

        // Registration fails again
        let result = client.try_register_asset(
            &valid_type,
            &String::from_str(&env, "More metadata"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidAssetType as u32
            )))
        );
    }

    #[test]
    fn test_batch_register_validates_asset_types() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("VALID"));

        let owner = Address::generate(&env);
        let mut batch = Vec::new(&env);
        batch.push_back(AssetInput {
            asset_type: symbol_short!("VALID"),
            metadata: String::from_str(&env, "Meta 1"),
            serial_number: unique_serial(&env),
        });
        batch.push_back(AssetInput {
            asset_type: symbol_short!("JUNK"),
            metadata: String::from_str(&env, "Meta 2"),
            serial_number: unique_serial(&env),
        });

        let result = client.try_batch_register_assets(&owner, &batch);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::InvalidAssetType as u32
            )))
        );
    }

    #[test]
    fn test_non_owner_cannot_deregister_asset() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        // A third party (neither admin nor owner) must be rejected
        let stranger = Address::generate(&env);
        let result = client.try_deregister_asset(&stranger, &id);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedOwner as u32
            )))
        );
        assert!(client.asset_exists(&id));
    }

    #[test]
    fn test_owner_can_deregister_own_asset() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        client.deregister_asset(&owner, &id);
        assert!(!client.asset_exists(&id));
    }

    #[test]
    fn test_deregister_asset_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        client.deregister_asset(&owner, &id);

        let events = env.events().all();
        let (_, topics, data): (_, soroban_sdk::Vec<soroban_sdk::Val>, soroban_sdk::Val) =
            events.last().unwrap();
        use soroban_sdk::IntoVal;
        let topic0: soroban_sdk::Val =
            <Symbol as IntoVal<Env, soroban_sdk::Val>>::into_val(&DEREG_TOPIC, &env);
        let topic1: soroban_sdk::Val = <u64 as IntoVal<Env, soroban_sdk::Val>>::into_val(&id, &env);
        assert_eq!(topics.get(0).unwrap().get_payload(), topic0.get_payload());
        assert_eq!(topics.get(1).unwrap().get_payload(), topic1.get_payload());
        let (emitted_type, emitted_owner): (Symbol, Address) =
            soroban_sdk::FromVal::from_val(&env, &data);
        assert_eq!(emitted_type, symbol_short!("GENSET"));
        assert_eq!(emitted_owner, owner);
    }

    #[test]
    fn test_deregister_asset_emits_dereg_topic() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        client.deregister_asset(&owner, &id);

        let events = env.events().all();
        let (_, topics, _): (_, soroban_sdk::Vec<soroban_sdk::Val>, soroban_sdk::Val) =
            events.last().unwrap();
        use soroban_sdk::TryIntoVal;
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        assert_eq!(
            t0,
            symbol_short!("DEREG"),
            "deregister_asset must emit DEREG topic (≤8 chars)"
        );
    }

    #[test]
    fn test_deregister_nonexistent_asset_returns_structured_error() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        assert_eq!(
            client.try_deregister_asset(&admin, &9999u64),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32
            )))
        );
    }

    #[test]
    fn test_remove_asset_type_blocked_while_assets_exist() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        // Removal must be rejected while the asset still exists
        assert_eq!(
            client.try_remove_asset_type(&admin, &symbol_short!("GENSET")),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::TypeInUse as u32
            )))
        );

        // Existing asset is still intact
        assert!(client.asset_exists(&id));
        assert!(client.is_valid_asset_type(&symbol_short!("GENSET")));

        // After deregistering the asset the type can be removed
        client.deregister_asset(&owner, &id);
        client.remove_asset_type(&admin, &symbol_short!("GENSET"));
        assert!(!client.is_valid_asset_type(&symbol_short!("GENSET")));
    }

    #[test]
    fn test_add_asset_type_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let events = env.events().all();
        let (_, topics, data): (_, soroban_sdk::Vec<soroban_sdk::Val>, soroban_sdk::Val) =
            events.last().unwrap();
        use soroban_sdk::IntoVal;
        let expected_topic: soroban_sdk::Val =
            <Symbol as IntoVal<Env, soroban_sdk::Val>>::into_val(&ADD_TYPE_TOPIC, &env);
        assert_eq!(
            topics.get(0).unwrap().get_payload(),
            expected_topic.get_payload()
        );
        let (emitted_type,): (Symbol,) = soroban_sdk::FromVal::from_val(&env, &data);
        assert_eq!(emitted_type, symbol_short!("GENSET"));
    }

    #[test]
    fn test_add_asset_type_emits_admin_audit_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        let timestamp = env.ledger().timestamp();
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let events = env.events().all();
        let (_, topics, data) = events.get(0).unwrap();
        let t0: Symbol = topics.get(0).unwrap().try_into_val(&env).unwrap();
        let t1: Symbol = topics.get(1).unwrap().try_into_val(&env).unwrap();
        assert_eq!(t0, symbol_short!("ADM_AUD"));
        assert_eq!(t1, symbol_short!("ADD_TYPE"));

        let (emitted_admin, emitted_timestamp, emitted_type): (Address, u64, Symbol) =
            data.try_into_val(&env).unwrap();
        assert_eq!(emitted_admin, admin);
        assert_eq!(emitted_timestamp, timestamp);
        assert_eq!(emitted_type, symbol_short!("GENSET"));
    }

    #[test]
    fn test_remove_asset_type_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));
        client.remove_asset_type(&admin, &symbol_short!("GENSET"));

        let events = env.events().all();
        let (_, topics, data): (_, soroban_sdk::Vec<soroban_sdk::Val>, soroban_sdk::Val) =
            events.last().unwrap();
        use soroban_sdk::IntoVal;
        let expected_topic: soroban_sdk::Val =
            <Symbol as IntoVal<Env, soroban_sdk::Val>>::into_val(&RM_TYPE_TOPIC, &env);
        assert_eq!(
            topics.get(0).unwrap().get_payload(),
            expected_topic.get_payload()
        );
        let (emitted_type,): (Symbol,) = soroban_sdk::FromVal::from_val(&env, &data);
        assert_eq!(emitted_type, symbol_short!("GENSET"));
    }

    #[test]
    fn test_register_asset_rejects_empty_metadata() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let owner = Address::generate(&env);
        let client = AssetRegistryClient::new(&env, &env.register(AssetRegistry, ()));
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let result = client.try_register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, ""),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(result, Err(Ok(ContractError::EmptyMetadata.into())));
    }

    #[test]
    fn test_asset_count_survives_instance_ttl_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let id1 = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Asset One"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(id1, 1);
        assert_eq!(client.asset_count(), 1);

        // Simulate instance storage TTL expiry by wiping instance keys
        env.as_contract(&contract_id, || {
            env.storage().instance().remove(&ADMIN_KEY);
        });

        // ASSET_COUNT lives in persistent storage — must still return 1
        assert_eq!(
            client.asset_count(),
            1,
            "asset_count must survive instance TTL expiry"
        );

        // Next registration must get ID 2, not 1 (no collision)
        let id2 = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Asset Two"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(
            id2, 2,
            "ID assignment must be consistent after instance TTL expiry"
        );
    }

    #[test]
    fn test_pause_state_persists_across_instance_ttl_boundary() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        // Pause the contract
        client.pause(&admin);
        assert!(client.is_paused());

        // Simulate instance TTL expiry by wiping instance storage
        env.as_contract(&contract_id, || {
            env.storage().instance().remove(&ADMIN_KEY);
            env.storage().instance().remove(&PENDING_ADMIN_KEY);
        });

        // PAUSED_KEY lives in persistent storage — must still be true
        assert!(
            client.is_paused(),
            "pause state must survive instance TTL expiry"
        );

        // Writes must still be blocked
        let owner = Address::generate(&env);
        assert_eq!(
            client.try_register_asset(
                &symbol_short!("GENSET"),
                &String::from_str(&env, "test asset"),
                &unique_serial(&env),
                &owner,
            ),
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::Paused as u32
            )))
        );
    }

    // --- Instance TTL expiry tests ---

    /// Helper: wipe instance storage to simulate TTL expiry.
    fn wipe_instance(env: &Env, contract_id: &Address) {
        env.as_contract(contract_id, || {
            env.storage().instance().remove(&ADMIN_KEY);
            env.storage().instance().remove(&PENDING_ADMIN_KEY);
        });
    }

    #[test]
    fn test_pause_extends_instance_ttl_after_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        // Simulate expiry then re-init
        client.initialize_admin(&admin, &admin);
        wipe_instance(&env, &contract_id);
        client.initialize_admin(&admin, &admin);

        client.pause(&admin);
        let ttl = env.as_contract(&contract_id, || env.storage().instance().get_ttl());
        assert!(ttl > 0, "pause must extend instance TTL");
    }

    #[test]
    fn test_unpause_extends_instance_ttl_after_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.pause(&admin);
        wipe_instance(&env, &contract_id);
        client.initialize_admin(&admin, &admin);

        client.unpause(&admin);
        let ttl = env.as_contract(&contract_id, || env.storage().instance().get_ttl());
        assert!(ttl > 0, "unpause must extend instance TTL");
    }

    #[test]
    fn test_propose_admin_extends_instance_ttl_after_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        wipe_instance(&env, &contract_id);
        client.initialize_admin(&admin, &admin);

        let new_admin = Address::generate(&env);
        client.propose_admin(&admin, &new_admin);
        let ttl = env.as_contract(&contract_id, || env.storage().instance().get_ttl());
        assert!(ttl > 0, "propose_admin must extend instance TTL");
    }

    #[test]
    fn test_accept_admin_extends_instance_ttl_after_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.propose_admin(&admin, &new_admin);

        // Simulate partial expiry (keep admin + pending_admin intact)
        // accept_admin reads PENDING_ADMIN_KEY which must still be present
        client.accept_admin(&new_admin);
        assert_eq!(client.get_admin(), new_admin);
        let ttl = env.as_contract(&contract_id, || env.storage().instance().get_ttl());
        assert!(ttl > 0, "accept_admin must extend instance TTL");
    }

    #[test]
    fn test_upgrade_extends_instance_ttl_after_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        wipe_instance(&env, &contract_id);
        client.initialize_admin(&admin, &admin);

        let hash = BytesN::from_array(&env, &[0xabu8; 32]);
        client.propose_upgrade(&admin, &hash);
        let base = env.ledger().timestamp();
        env.ledger().set_timestamp(base + TIMELOCK_DELAY_SECS + 1);
        client.execute_upgrade(&admin);
        let ttl = env.as_contract(&contract_id, || env.storage().instance().get_ttl());
        assert!(ttl > 0, "upgrade must extend instance TTL");
    }

    #[test]
    fn test_admin_ops_work_after_instance_ttl_expiry_and_reinit() {
        // Full scenario: instance expires, admin re-initializes, all ops succeed.
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        // Simulate instance TTL expiry
        wipe_instance(&env, &contract_id);

        // Re-initialize admin
        client.initialize_admin(&admin, &admin);

        // All admin ops must succeed and extend TTL
        client.pause(&admin);
        client.unpause(&admin);

        let new_admin = Address::generate(&env);
        client.propose_admin(&admin, &new_admin);
        client.accept_admin(&new_admin);
        assert_eq!(client.get_admin(), new_admin);

        let ttl = env.as_contract(&contract_id, || env.storage().instance().get_ttl());
        assert!(ttl > 0, "instance TTL must be live after admin ops");
    }

    // --- Issue #381: is_valid_asset_type survives instance TTL expiry ---

    #[test]
    fn test_is_valid_asset_type_survives_instance_ttl_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        // Simulate instance TTL expiry by wiping all instance storage
        env.as_contract(&contract_id, || {
            env.storage().instance().remove(&ADMIN_KEY);
        });

        // Asset type lives in persistent storage — must still be valid
        assert!(
            client.is_valid_asset_type(&symbol_short!("GENSET")),
            "asset type must remain valid after instance TTL expiry"
        );
    }

    // --- Issue #382: add_asset_type and remove_asset_type extend TTL ---

    #[test]
    fn test_add_asset_type_extends_persistent_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        env.as_contract(&contract_id, || {
            let ttl = env
                .storage()
                .persistent()
                .get_ttl(&asset_type_key(&symbol_short!("GENSET")));
            assert!(
                ttl > 0,
                "asset type key TTL must be extended after add_asset_type"
            );
        });
    }

    // --- Issue #383: get_assets_by_owner extends TTL on read ---

    #[test]
    fn test_get_assets_by_owner_extends_ttl_on_read() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        // Read via get_assets_by_owner — TTL must be extended
        client.get_assets_by_owner(&owner);

        env.as_contract(&contract_id, || {
            let ttl = env.storage().persistent().get_ttl(&owner_index_key(&owner));
            assert!(ttl > 0, "owner index TTL must be extended on read");
        });
    }

    #[test]
    #[ignore = "re-entry: asset_registry -> lifecycle -> asset_registry is not allowed in Soroban"]
    fn test_get_lifecycle_score_cross_contract_call() {
        let env = Env::default();
        env.mock_all_auths();

        let asset_registry_id = env.register(AssetRegistry, ());
        let engineer_registry_id = env.register(engineer_registry::EngineerRegistry, ());
        let lifecycle_id = env.register(lifecycle::Lifecycle, ());

        let asset_client = AssetRegistryClient::new(&env, &asset_registry_id);
        let lifecycle_client = lifecycle::LifecycleClient::new(&env, &lifecycle_id);

        let admin = Address::generate(&env);
        let asset_owner = Address::generate(&env);

        // Initialize both contracts
        asset_client.initialize_admin(&admin, &admin);
        asset_client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let lifecycle_admin = Address::generate(&env);
        let engineer_registry_id = Address::generate(&env);
        let deployer = Address::generate(&env);
        lifecycle_client.initialize(
            &deployer,
            &asset_registry_id,
            &engineer_registry_id,
            &lifecycle_admin,
            &200,
        );

        // Register an asset
        let asset_id = asset_client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Test Asset"),
            &unique_serial(&env),
            &asset_owner,
        );

        // Get lifecycle score via cross-contract call
        let score = asset_client.get_lifecycle_score(&asset_id, &lifecycle_id);

        // Score should be a valid u32 (initially 0 for new asset)
        assert_eq!(score, 0);
    }

    #[test]
    fn test_get_lifecycle_score_nonexistent_asset() {
        let env = Env::default();
        env.mock_all_auths();

        let asset_registry_id = env.register(AssetRegistry, ());
        let lifecycle_id = env.register(lifecycle::Lifecycle, ());

        let asset_client = AssetRegistryClient::new(&env, &asset_registry_id);

        let admin = Address::generate(&env);
        asset_client.initialize_admin(&admin, &admin);

        // Try to get lifecycle score for non-existent asset
        let result = asset_client.try_get_lifecycle_score(&999, &lifecycle_id);

        // Should return error for non-existent asset
        assert!(result.is_err());
    }

    // --- Issue #384: initialize_admin extends instance TTL after writing ADMIN_KEY ---

    #[test]
    fn test_admin_key_survives_ttl_boundary() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        // Verify instance TTL was extended after writing ADMIN_KEY
        env.as_contract(&contract_id, || {
            let ttl = env.storage().instance().get_ttl();
            assert!(
                ttl > 0,
                "instance TTL must be extended after initialize_admin"
            );
        });

        // Simulate TTL boundary: advance ledger sequence past the minimum TTL
        // then verify get_admin still returns the correct admin
        env.ledger().with_mut(|li| {
            li.sequence_number += TTL_THRESHOLD;
            li.timestamp += (TTL_THRESHOLD as u64) * 5;
        });

        // get_admin must still resolve correctly (TTL was extended at init time)
        assert_eq!(client.get_admin(), admin);
    }

    /// Regression test: type_count must survive instance TTL expiry.
    ///
    /// Before the fix, type_count was stored in instance storage. If instance
    /// storage expired, remove_asset_type would read 0 and incorrectly allow
    /// removal of a type that still has registered assets.
    ///
    /// After the fix, type_count is in persistent storage. Advancing the ledger
    /// sequence past the instance TTL window must not affect the count, and
    /// remove_asset_type must still be blocked.
    #[test]
    fn test_remove_asset_type_blocked_after_instance_ttl_boundary() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "CAT-3516"),
            &unique_serial(&env),
            &owner,
        );

        // Verify the count is in persistent storage (not instance).
        let persistent_count: u64 = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get(&type_count_key(&symbol_short!("GENSET")))
                .unwrap_or(0)
        });
        assert_eq!(
            persistent_count, 1,
            "type count must be in persistent storage"
        );

        // Advance ledger sequence well past the instance TTL window.
        // In the old code this would cause instance storage to return 0,
        // allowing remove_asset_type to succeed incorrectly.
        env.ledger().with_mut(|li| {
            li.sequence_number += 518400 + 1;
            li.timestamp += (518400 + 1) * 5;
        });

        // remove_asset_type must still be blocked because the asset still exists.
        let result = client.try_remove_asset_type(&admin, &symbol_short!("GENSET"));
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::TypeInUse as u32
            ))),
            "remove_asset_type must be blocked when assets of that type exist"
        );
    }

    #[test]
    fn test_initialize_admin_rejects_non_deployer() {
        let env = Env::default();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let deployer = Address::generate(&env);
        let attacker = Address::generate(&env);

        // Authorize only the attacker, not the deployer.
        use soroban_sdk::IntoVal;
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &attacker,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "initialize_admin",
                args: (&attacker, &attacker).into_val(&env),
                sub_invokes: &[],
            },
        }]);

        // Passing attacker as deployer but deployer's auth is not present — must fail.
        let result = client.try_initialize_admin(&deployer, &attacker);
        assert!(
            result.is_err(),
            "non-deployer must not be able to initialize"
        );
    }

    fn setup_with_types(env: &Env) -> (AssetRegistryClient, Address, Address) {
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(env, &contract_id);
        let admin = Address::generate(env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));
        client.add_asset_type(&admin, &symbol_short!("TURBINE"));
        (client, admin, Address::generate(env))
    }

    #[test]
    fn test_get_assets_by_type_registration() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, owner) = setup_with_types(&env);

        let id1 = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator A"),
            &unique_serial(&env),
            &owner,
        );
        let id2 = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator B"),
            &unique_serial(&env),
            &owner,
        );
        client.register_asset(
            &symbol_short!("TURBINE"),
            &String::from_str(&env, "Turbine X"),
            &unique_serial(&env),
            &owner,
        );

        let gensets = client.get_assets_by_type(&symbol_short!("GENSET"));
        assert_eq!(gensets.len(), 2);
        assert_eq!(gensets.get(0).unwrap(), id1);
        assert_eq!(gensets.get(1).unwrap(), id2);

        let turbines = client.get_assets_by_type(&symbol_short!("TURBINE"));
        assert_eq!(turbines.len(), 1);
    }

    #[test]
    fn test_get_assets_by_type_after_deregister() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin, owner) = setup_with_types(&env);

        let id1 = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator A"),
            &unique_serial(&env),
            &owner,
        );
        let id2 = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator B"),
            &unique_serial(&env),
            &owner,
        );

        client.deregister_asset(&admin, &id1);

        let gensets = client.get_assets_by_type(&symbol_short!("GENSET"));
        assert_eq!(gensets.len(), 1);
        assert_eq!(gensets.get(0).unwrap(), id2);
    }

    #[test]
    fn test_get_assets_by_type_page() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, owner) = setup_with_types(&env);

        let metas = [
            "Generator 0",
            "Generator 1",
            "Generator 2",
            "Generator 3",
            "Generator 4",
        ];
        let mut ids: Vec<u64> = Vec::new(&env);
        for meta in metas.iter() {
            ids.push_back(client.register_asset(
                &symbol_short!("GENSET"),
                &String::from_str(&env, meta),
                &unique_serial(&env),
                &owner,
            ));
        }

        // Page 0: first 2
        let page0 = client.get_assets_by_type_page(&symbol_short!("GENSET"), &0, &2);
        assert_eq!(page0.len(), 2);
        assert_eq!(page0.get(0).unwrap(), ids.get(0).unwrap());
        assert_eq!(page0.get(1).unwrap(), ids.get(1).unwrap());

        // Page 1: next 2
        let page1 = client.get_assets_by_type_page(&symbol_short!("GENSET"), &2, &2);
        assert_eq!(page1.len(), 2);
        assert_eq!(page1.get(0).unwrap(), ids.get(2).unwrap());

        // Last page: 1 item
        let page2 = client.get_assets_by_type_page(&symbol_short!("GENSET"), &4, &2);
        assert_eq!(page2.len(), 1);

        // Out-of-bounds offset returns empty
        let empty = client.get_assets_by_type_page(&symbol_short!("GENSET"), &10, &2);
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn test_get_assets_by_type_batch_register() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, owner) = setup_with_types(&env);

        let assets = soroban_sdk::vec![
            &env,
            AssetInput {
                asset_type: symbol_short!("GENSET"),
                metadata: String::from_str(&env, "Generator Batch 1"),
                serial_number: unique_serial(&env),
            },
            AssetInput {
                asset_type: symbol_short!("GENSET"),
                metadata: String::from_str(&env, "Generator Batch 2"),
                serial_number: unique_serial(&env),
            },
            AssetInput {
                asset_type: symbol_short!("TURBINE"),
                metadata: String::from_str(&env, "Turbine Batch 1"),
                serial_number: unique_serial(&env),
            },
        ];

        client.batch_register_assets(&owner, &assets);

        let gensets = client.get_assets_by_type(&symbol_short!("GENSET"));
        assert_eq!(gensets.len(), 2);

        let turbines = client.get_assets_by_type(&symbol_short!("TURBINE"));
        assert_eq!(turbines.len(), 1);
    }

    #[test]
    fn test_asset_status_active() {
    fn test_get_asset_count() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let asset_id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Active Generator"),
            &unique_serial(&env),
            &owner,
        );

        let status = client.asset_status(&asset_id);
        assert_eq!(status, AssetStatus::Active);
    }

    #[test]
    fn test_asset_status_decommissioned() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let asset_id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Decomm Generator"),
            &unique_serial(&env),
            &owner,
        );

        // Manually set the decommissioned flag
        let key = decommissioned_key(asset_id);
        env.storage().persistent().set(&key, &true);

        let status = client.asset_status(&asset_id);
        assert_eq!(status, AssetStatus::Decommissioned);
    }

    #[test]
    fn test_asset_status_not_found() {
        let env = Env::default();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let result = client.try_asset_status(&999u64);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32
            )))
        );
    }

    #[test]
    fn test_asset_status_under_maintenance() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let asset_id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Maintained Generator"),
            &unique_serial(&env),
            &owner,
        );

        // Manually set the under_maintenance flag
        let key = (symbol_short!("U_MAINT"), asset_id);
        env.storage().persistent().set(&key, &true);

        let status = client.asset_status(&asset_id);
        assert_eq!(status, AssetStatus::UnderMaintenance);
    }

    #[test]
    fn test_decommission_asset_admin_can_decommission() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let asset_id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Decomm Test"),
            &unique_serial(&env),
            &owner,
        );

        // Decommission the asset
        client.decommission_asset(&admin, &asset_id);

        // Verify status is Decommissioned
        let status = client.asset_status(&asset_id);
        assert_eq!(status, AssetStatus::Decommissioned);
    }

    #[test]
    fn test_decommission_asset_non_admin_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let asset_id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Decomm Test"),
            &unique_serial(&env),
            &owner,
        );

        // Non-admin tries to decommission
        let non_admin = Address::generate(&env);
        let result = client.try_decommission_asset(&non_admin, &asset_id);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::UnauthorizedAdmin as u32
            )))
        );
    }

    #[test]
    fn test_decommission_nonexistent_asset() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        // Try to decommission non-existent asset
        let result = client.try_decommission_asset(&admin, &999u64);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::AssetNotFound as u32
            )))
        );
    }

    #[test]
    fn test_decommission_asset_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let asset_id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Event Test"),
            &unique_serial(&env),
            &owner,
        );

        // Decommission the asset and check for event
        client.decommission_asset(&admin, &asset_id);

        let events = env.events().all();
        // Should have at least one DECOMM event
        assert!(events.len() > 0, "decommission_asset should emit an event");
        // Counter starts at 0
        assert_eq!(client.get_asset_count(), 0);

        let owner = Address::generate(&env);

        // Register first asset, count should be 1
        client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator 1"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(client.get_asset_count(), 1);

        // Register second asset, count should be 2
        client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator 2"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(client.get_asset_count(), 2);

        // Register third asset, count should be 3
        client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator 3"),
            &unique_serial(&env),
            &owner,
        );
        assert_eq!(client.get_asset_count(), 3);
    }

    // --- Issue: get_assets_by_type_paginated tests ---

    fn setup_with_types_for_pagination(env: &Env) -> (AssetRegistryClient, Address) {
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(env, &contract_id);
        let admin = Address::generate(env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));
        client.add_asset_type(&admin, &symbol_short!("TURBINE"));
        let owner = Address::generate(env);
        (client, owner)
    }

    #[test]
    fn test_get_assets_by_type_paginated_standard() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, owner) = setup_with_types_for_pagination(&env);

        for i in 0..7u32 {
            client.register_asset(
                &symbol_short!("GENSET"),
                &String::from_str(&env, &std::format!("Generator {i}")),
                &unique_serial(&env),
                &owner,
            );
        }

        // Page 0: items 0-2
        let p0 = client.get_assets_by_type_paginated(&symbol_short!("GENSET"), &0, &3);
        assert_eq!(p0.total, 7);
        assert_eq!(p0.assets.len(), 3);

        // Page 1: items 3-5
        let p1 = client.get_assets_by_type_paginated(&symbol_short!("GENSET"), &1, &3);
        assert_eq!(p1.total, 7);
        assert_eq!(p1.assets.len(), 3);

        // Page 2: item 6 (last page, partial)
        let p2 = client.get_assets_by_type_paginated(&symbol_short!("GENSET"), &2, &3);
        assert_eq!(p2.total, 7);
        assert_eq!(p2.assets.len(), 1);
    }

    #[test]
    fn test_get_assets_by_type_paginated_empty_type() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _) = setup_with_types_for_pagination(&env);

        // No assets of type TURBINE registered
        let result = client.get_assets_by_type_paginated(&symbol_short!("TURBINE"), &0, &10);
        assert_eq!(result.total, 0);
        assert_eq!(result.assets.len(), 0);
    }

    #[test]
    fn test_get_assets_by_type_paginated_out_of_bounds() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, owner) = setup_with_types_for_pagination(&env);

        client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Generator 0"),
            &unique_serial(&env),
            &owner,
        );

        // Page beyond the end returns empty assets but correct total
        let result = client.get_assets_by_type_paginated(&symbol_short!("GENSET"), &5, &10);
        assert_eq!(result.total, 1);
        assert_eq!(result.assets.len(), 0);
    }

    #[test]
    fn test_get_assets_by_type_paginated_page_size_capped_at_100() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, owner) = setup_with_types_for_pagination(&env);

        for i in 0..50u32 {
            client.register_asset(
                &symbol_short!("GENSET"),
                &String::from_str(&env, &std::format!("Generator {i}")),
                &unique_serial(&env),
                &owner,
            );
        }

        // page_size=200 is capped to 100, so at most 100 assets returned
        let result = client.get_assets_by_type_paginated(&symbol_short!("GENSET"), &0, &200);
        assert_eq!(result.total, 50);
        assert_eq!(result.assets.len(), 50); // only 50 assets exist
    }

    // --- #751: dedup key includes asset_type ---

    #[test]
    fn test_same_metadata_different_type_is_allowed() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));
        client.add_asset_type(&admin, &symbol_short!("TURBINE"));

        let owner = Address::generate(&env);
        let metadata = String::from_str(&env, "Spec v1");

        // Same metadata, different asset types — both should succeed
        let id1 = client.register_asset(&symbol_short!("GENSET"), &metadata, &unique_serial(&env), &owner);
        let id2 = client.register_asset(&symbol_short!("TURBINE"), &metadata, &unique_serial(&env), &owner);

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_same_owner_same_type_same_metadata_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let metadata = String::from_str(&env, "Spec v1");

        client.register_asset(&symbol_short!("GENSET"), &metadata, &unique_serial(&env), &owner);

        let result = client.try_register_asset(&symbol_short!("GENSET"), &metadata, &unique_serial(&env), &owner);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::DuplicateAsset as u32,
            ))),
        );
    }

    #[test]
    fn test_batch_same_metadata_different_type_is_allowed() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));
        client.add_asset_type(&admin, &symbol_short!("TURBINE"));

        let owner = Address::generate(&env);
        let mut batch = Vec::new(&env);
        batch.push_back(AssetInput {
            asset_type: symbol_short!("GENSET"),
            metadata: String::from_str(&env, "Shared spec"),
            serial_number: unique_serial(&env),
        });
        batch.push_back(AssetInput {
            asset_type: symbol_short!("TURBINE"),
            metadata: String::from_str(&env, "Shared spec"),
            serial_number: unique_serial(&env),
        });

        let ids = client.batch_register_assets(&owner, &batch);
        assert_eq!(ids.len(), 2);
    }

    // --- #752: upgrade timelock tests ---

    #[test]
    fn test_execute_upgrade_before_timelock_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        let hash = BytesN::from_array(&env, &[0xabu8; 32]);
        client.propose_upgrade(&admin, &hash);

        // Not enough time passed — should fail
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
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        let hash = BytesN::from_array(&env, &[0xabu8; 32]);
        client.propose_upgrade(&admin, &hash);

        let base = env.ledger().timestamp();
        env.ledger().set_timestamp(base + TIMELOCK_DELAY_SECS + 1);

        // Should succeed
        client.execute_upgrade(&admin);
    }

    #[test]
    fn test_execute_upgrade_without_proposal_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        let result = client.try_execute_upgrade(&admin);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::ProposalNotFound as u32,
            ))),
        );
    }

    #[test]
    fn test_propose_upgrade_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        let hash = BytesN::from_array(&env, &[0xabu8; 32]);
        client.propose_upgrade(&admin, &hash);

        let events = env.events().all();
        use soroban_sdk::TryIntoVal;
        let prop_event = events.iter().find(|(_, topics, _)| {
            if let Some(val) = topics.get(0) {
                if let Ok(s) = val.try_into_val::<_, Symbol>(&env) {
                    return s == symbol_short!("PROP_UPG");
                }
            }
            false
        });
        assert!(prop_event.is_some(), "PROP_UPG event must be emitted on propose_upgrade");
    }

    #[test]
    fn test_upgrade_emit_event_after_execute() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);

        let hash = BytesN::from_array(&env, &[0xabu8; 32]);
        client.propose_upgrade(&admin, &hash);
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
        assert!(upgrade_event.is_some(), "UPGRADE event must be emitted on execute_upgrade");
        let (_, _, data) = upgrade_event.unwrap();
        let emitted_hash: BytesN<32> = data.try_into_val(&env).unwrap();
        assert_eq!(emitted_hash, hash);
    }

    #[test]
    fn test_get_assets_by_type_paginated_total_matches_across_pages() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, owner) = setup_with_types_for_pagination(&env);

        for i in 0..12u32 {
            client.register_asset(
                &symbol_short!("GENSET"),
                &String::from_str(&env, &std::format!("Generator {i}")),
                &unique_serial(&env),
                &owner,
            );
        }

        // Total reported on every page must be the same
        let p0 = client.get_assets_by_type_paginated(&symbol_short!("GENSET"), &0, &5);
        let p1 = client.get_assets_by_type_paginated(&symbol_short!("GENSET"), &1, &5);
        let p2 = client.get_assets_by_type_paginated(&symbol_short!("GENSET"), &2, &5);
        assert_eq!(p0.total, 12);
        assert_eq!(p1.total, 12);
        assert_eq!(p2.total, 12);

        // Pages cover all 12 assets without overlap: 5 + 5 + 2 = 12
        assert_eq!(p0.assets.len() + p1.assets.len() + p2.assets.len(), 12);
    }
}

    // --- Issue: Block re-proposal of deregister timelock ---

    #[test]
    fn test_propose_deregister_cannot_overwrite_pending_proposal() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(AssetRegistry, ());
        let client = AssetRegistryClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_admin(&admin, &admin);
        client.add_asset_type(&admin, &symbol_short!("GENSET"));

        let owner = Address::generate(&env);
        let asset_id = client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Propose Block Test"),
            &unique_serial(&env),
            &owner,
        );

        // First proposal succeeds
        client.propose_deregister_asset(&owner, &asset_id);

        // Second proposal with a pending one must fail
        let result = client.try_propose_deregister_asset(&owner, &asset_id);
        assert_eq!(
            result,
            Err(Ok(soroban_sdk::Error::from_contract_error(
                ContractError::ProposalAlreadyExists as u32
            )))
        );
    }

    // --- Issue: decommission_asset_notify freezes lifecycle score ---

    #[test]
    fn test_decommission_asset_notify_freezes_lifecycle_score() {
        let env = Env::default();
        env.mock_all_auths();

        // Set up all three contracts
        let asset_registry_id = env.register(AssetRegistry, ());
        let engineer_registry_id = env.register(engineer_registry::EngineerRegistry, ());
        let lifecycle_id = env.register(lifecycle::Lifecycle, ());

        let asset_client = AssetRegistryClient::new(&env, &asset_registry_id);
        let eng_client = engineer_registry::EngineerRegistryClient::new(&env, &engineer_registry_id);
        let lc_client = lifecycle::LifecycleClient::new(&env, &lifecycle_id);

        let asset_admin = Address::generate(&env);
        let lc_admin = Address::generate(&env);
        let eng_admin = Address::generate(&env);

        asset_client.initialize_admin(&asset_admin, &asset_admin);
        asset_client.add_asset_type(&asset_admin, &symbol_short!("GENSET"));

        eng_client.initialize_admin(&eng_admin, &eng_admin);

        lc_client.initialize(
            &lc_admin,
            &asset_registry_id,
            &engineer_registry_id,
            &lc_admin,
            &0u32,
        );

        // Register asset and engineer, then submit maintenance
        let owner = Address::generate(&env);
        let asset_id = asset_client.register_asset(
            &symbol_short!("GENSET"),
            &String::from_str(&env, "Freeze Score Test"),
            &String::from_str(&env, "SN-FREEZE-001"),
            &owner,
        );
        let issuer = Address::generate(&env);
        let engineer = Address::generate(&env);
        eng_client.add_trusted_issuer(&eng_admin, &issuer);
        eng_client.register_engineer(
            &engineer,
            &soroban_sdk::BytesN::from_array(&env, &[1u8; 32]),
            &issuer,
            &31_536_000u64,
        );
        lc_client.authorize_engineer(&owner, &asset_id, &engineer);
        lc_client.submit_maintenance(
            &asset_id,
            &symbol_short!("OIL_CHG"),
            &String::from_str(&env, "Pre-decommission service"),
            &engineer,
        );

        let score_at_decommission = lc_client.get_collateral_score(&asset_id);
        assert!(score_at_decommission > 0, "score must be non-zero before decommission");

        // Decommission and notify lifecycle
        asset_client.decommission_asset_notify(&asset_admin, &asset_id, &lifecycle_id);

        // Advance time past several decay intervals
        env.ledger().with_mut(|li| li.timestamp += 50_000_000);

        // Score must be frozen — no decay after decommission
        let score_after = lc_client.get_collateral_score(&asset_id);
        assert_eq!(
            score_after, score_at_decommission,
            "lifecycle score must not decay after decommission_asset_notify"
        );
    }
}
