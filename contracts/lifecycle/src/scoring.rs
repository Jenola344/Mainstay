#![no_std]

use crate::errors::ContractError;
use crate::types::{Config, MaintenanceRecord, ScoreEntry};
use soroban_sdk::{panic_with_error, symbol_short, Env, Symbol, Vec};

pub fn score_history_push(env: &Env, asset_id: u64, entry: ScoreEntry, max_history: u32) {
    let key = super::score_history_key(asset_id);
    let mut history: Vec<ScoreEntry> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| Vec::new(env));

    // Deduplicate: if the last entry shares the same ledger timestamp, update it in-place
    // instead of appending. This prevents multiple submissions in the same ledger from
    // inflating history length and skewing trend analysis.
    let last_idx = history.len().saturating_sub(1);
    if !history.is_empty() {
        let last = history.get(last_idx).unwrap();
        if last.timestamp == entry.timestamp {
            history.set(last_idx, entry);
            env.storage().persistent().set(&key, &history);
            env.storage()
                .persistent()
                .extend_ttl(&key, super::TTL_THRESHOLD, super::TTL_TARGET);
            return;
        }
    }

    if max_history > 0 && history.len() >= max_history {
        history.remove(0);
    }
    history.push_back(entry);
    env.storage().persistent().set(&key, &history);
    env.storage()
        .persistent()
        .extend_ttl(&key, super::TTL_THRESHOLD, super::TTL_TARGET);
}

pub fn get_task_weight(env: &Env, task_type: &Symbol) -> u32 {
    if task_type == &symbol_short!("OIL_CHG")
        || task_type == &symbol_short!("LUBE")
        || task_type == &symbol_short!("INSPECT")
    {
        return 2;
    }
    if task_type == &symbol_short!("FILTER")
        || task_type == &symbol_short!("TUNE_UP")
        || task_type == &symbol_short!("BRAKE")
    {
        return 5;
    }
    if task_type == &symbol_short!("ENGINE")
        || task_type == &symbol_short!("OVERHAUL")
        || task_type == &symbol_short!("REBUILD")
    {
        return 10;
    }
    panic_with_error!(env, ContractError::InvalidTaskType);
}

pub fn compute_decay(env: &Env, asset_id: u64) -> u32 {
    let history: Vec<MaintenanceRecord> = env
        .storage()
        .persistent()
        .get(&super::history_key(asset_id))
        .unwrap_or(Vec::new(env));

    if history.is_empty() {
        return 0;
    }

    let config: Config = env
        .storage()
        .persistent()
        .get(&super::CONFIG)
        .unwrap_or_else(|| panic_with_error!(env, ContractError::NotInitialized));

    let current_time_seconds = env.ledger().timestamp();
    let current_ledger = current_time_seconds / 5;
    let mut total_score: u32 = 0;

    for record in history.iter() {
        let record_ledger = record.timestamp / 5;
        let age_ledgers = current_ledger.saturating_sub(record_ledger);
        let recency_weight = if age_ledgers >= super::MAX_AGE_LEDGERS {
            0u64
        } else {
            super::MAX_AGE_LEDGERS - age_ledgers
        };
        let base_score = config.score_increment as u64;
        let contribution = (base_score * recency_weight) / super::MAX_AGE_LEDGERS;
        total_score = total_score
            .checked_add(contribution as u32)
            .unwrap_or_else(|| panic_with_error!(env, ContractError::ScoreOverflow));
    }
    total_score.min(100)
}

pub fn apply_decay(
    env: &Env,
    asset_id: u64,
    emit_event: bool,
    update_on_zero_interval: bool,
    max_history: u32,
) -> u32 {
    let current_score: u32 = env
        .storage()
        .persistent()
        .get(&super::score_key(asset_id))
        .unwrap_or(0u32);

    if current_score == 0 {
        if env.storage().persistent().has(&super::last_update_key(asset_id)) {
            env.storage().persistent().extend_ttl(
                &super::last_update_key(asset_id),
                super::TTL_THRESHOLD,
                super::TTL_TARGET,
            );
        }
        return 0;
    }

    let last_update: u64 = env
        .storage()
        .persistent()
        .get(&super::last_update_key(asset_id))
        .unwrap_or(0u64);

    let config: Config = env
        .storage()
        .persistent()
        .get(&super::CONFIG)
        .unwrap_or_else(|| panic_with_error!(env, ContractError::NotInitialized));

    let current_time = env.ledger().timestamp();
    let time_elapsed = current_time.saturating_sub(last_update);
    let decay_intervals = time_elapsed / config.decay_interval;
    if decay_intervals == 0 && !update_on_zero_interval {
        env.storage()
            .persistent()
            .extend_ttl(&super::score_key(asset_id), super::TTL_THRESHOLD, super::TTL_TARGET);
        env.storage().persistent().extend_ttl(
            &super::last_update_key(asset_id),
            super::TTL_THRESHOLD,
            super::TTL_TARGET,
        );
        return current_score;
    }

    let total_decay = (decay_intervals as u32) * config.decay_rate;
    let new_score = current_score.saturating_sub(total_decay);

    env.storage()
        .persistent()
        .set(&super::score_key(asset_id), &new_score);
    env.storage()
        .persistent()
        .extend_ttl(&super::score_key(asset_id), super::TTL_THRESHOLD, super::TTL_TARGET);
    env.storage()
        .persistent()
        .set(&super::last_update_key(asset_id), &current_time);
    env.storage()
        .persistent()
        .extend_ttl(&super::last_update_key(asset_id), super::TTL_THRESHOLD, super::TTL_TARGET);

    score_history_push(
        env,
        asset_id,
        ScoreEntry {
            timestamp: current_time,
            score: new_score,
        },
        max_history,
    );

    if emit_event {
        env.events().publish(
            (super::EVENT_DECAY, asset_id),
            (current_score, new_score, current_time),
        );
    }

    new_score
}
