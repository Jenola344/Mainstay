#![no_std]

use soroban_sdk::{contracttype, Address, String, Symbol, Map};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceRecord {
    pub asset_id: u64,
    pub task_type: Symbol,
    pub notes: String,
    pub engineer: Address,
    pub timestamp: u64,
}

/// A point-in-time snapshot of the collateral score, recorded at each maintenance event.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScoreEntry {
    pub timestamp: u64,
    pub score: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchRecord {
    pub task_type: Symbol,
    pub notes: String,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub max_history: u32,
    pub score_increment: u32,
    pub decay_rate: u32,
    pub decay_interval: u64,
    pub eligibility_threshold: u32,
    pub max_notes_length: u32,
    pub task_weights: Map<Symbol, u32>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelockProposal {
    pub proposed_at: u64,
    pub executed: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum DataKey {
    AssetRegistry,
    EngineerRegistry,
    Config,
    Paused,
    PendingAdmin,
    History(u64),
    Score(u64),
    ScoreHistory(u64),
    LastUpdate(u64),
    EngineerHistory(Address),
    EngineerAuth(u64, Address),
    Timelock(Symbol),
}
