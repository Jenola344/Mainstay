# Soroban Storage & TTL Strategy

Soroban persistent storage entries expire if their Time-To-Live (TTL) is not extended. To prevent silent data loss, Mainstay contracts follow a standardized TTL management approach.

## Storage Types

- **Instance Storage**: Used for shared contract configuration (admin address, trusted issuers, registry bindings, etc.). Instance storage TTL is **not** automatically extended on every call — it must be explicitly extended on every write to prevent the admin address and other critical config from expiring.
- **Persistent Storage**: Used for all asset-specific data, maintenance records, and scores. **Requires explicit extension** to ensure longevity.

## TTL Parameters

Mainstay uses a standardized 30-day extension policy:
- **Threshold**: 518,400 ledgers (~30 days at 5s/ledger)
- **Target**: 518,400 ledgers (~30 days)

## Contract Storage Keys

### 1. Asset Registry

| Key Pattern | Storage Type | Description |
| ----------- | ------------ | ----------- |
| `(Symbol("ASSET"), id: u64)` | Persistent | Full `Asset` record (metadata, owner, etc.) |
| `(Symbol("DEDUP"), owner: Address, hash: BytesN<32>)` | Persistent | Mapping of unique metadata to active asset IDs |
| `Symbol("A_COUNT")` | Persistent | Global counter for total registered assets |
| `Symbol("PAUSED")` | Persistent | Contract pause flag |
| `Symbol("ADMIN")` | Instance | Admin address authorized for admin operations |
| `Symbol("PADMIN")` | Instance | Pending admin address during 2-step transfer |
| `(Symbol("AST_TYPE"), asset_type: Symbol)` | Persistent | Asset type allowlist entries |
| `(Symbol("AST_CNT"), asset_type: Symbol)` | Instance | Per-type asset count (for TypeInUse guard) |
| `(Symbol("OWN_IDX"), owner: Address)` | Persistent | Owner → Vec<asset_id> index |

### 2. Engineer Registry

| Key Pattern | Storage Type | Description |
| ----------- | ------------ | ----------- |
| `(Symbol("ENG"), addr: Address)` | Persistent | `Engineer` record (credential hash, active status) |
| `(Symbol("ISS_ENGS"), issuer: Address)` | Persistent | Issuer → Vec<engineer_address> mapping |
| `Symbol("PAUSED")` | Persistent | Contract pause flag |
| `(Symbol("TRUSTED"), issuer: Address)` | Instance | Registry of trusted credential issuers |
| `Symbol("ISS_LIST")` | Instance | Authoritative list of all trusted issuer addresses |
| `Symbol("ADMIN")` | Instance | Admin address authorized for trust management |
| `Symbol("PADMIN")` | Instance | Pending admin address during 2-step transfer |

### 3. Lifecycle Contract

All Lifecycle keys are stored in **persistent** storage. There is no instance storage in the Lifecycle contract — every key must be individually extended.

| Key Pattern | Extension Policy | Description |
| ----------- | ---------------- | ----------- |
| `(Symbol("HIST"), asset_id: u64)` | Extended on every `submit_maintenance` / `batch_submit_maintenance` write | `Vec<MaintenanceRecord>` of all verified events |
| `(Symbol("SCORE"), asset_id: u64)` | Extended on every score write (`submit_maintenance`, `decay_score`, `get_collateral_score`) | Current accumulated collateral score (0–100) |
| `(Symbol("SCHIST"), asset_id: u64)` | Extended alongside every score write | `Vec<ScoreEntry>` snapshots — (timestamp, score) per maintenance event |
| `(Symbol("LUPD"), asset_id: u64)` | Extended on every score write | Timestamp of the last maintenance submission or decay event |
| `Symbol("REGISTRY")` | Extended once at `initialize` | Linked Asset Registry contract address |
| `Symbol("ENG_REG")` | Extended once at `initialize` | Linked Engineer Registry contract address |
| `Symbol("CONFIG")` | Extended at `initialize` and every `update_config` | `Config` record (max history, decay rate/interval, eligibility threshold, task weights) |
| `Symbol("PAUSED")` | Extended on every `pause` and `unpause` | Contract pause flag |
| `Symbol("PADMIN")` | Extended on `propose_admin`; removed on `accept_admin` | Pending admin address during 2-step transfer |
| `(Symbol("ENG_HIST"), engineer: Address)` | Extended on every `submit_maintenance` that records a new asset for the engineer | Engineer → `Vec<asset_id>` association list |
| `(Symbol("ENG_AUTH"), asset_id: u64, engineer: Address)` | Extended on `authorize_engineer` | Owner-granted authorization flag for a specific (asset, engineer) pair |
| `(Symbol("TL_PROP"), op: Symbol)` | Extended on `propose_*`; removed on execution or cancellation | `TimelockProposal` for admin configuration changes |
| `(Symbol("FROZEN"), asset_id: u64)` | Extended on `decommission_notify` | Flag indicating the asset's score has been frozen at decommission time |
| `(Symbol("FRZ_SCR"), asset_id: u64)` | Extended on `decommission_notify` | Score captured at decommission time — returned by `get_collateral_score` for frozen assets |
| `(Symbol("HLTH_SNP"), asset_id: u64)` | Extended on every `take_health_snapshot` | `Vec<HealthSnapshot>` — cumulative health snapshot history |
| `(Symbol("RVK_TL"), asset_id: u64, engineer: Address)` | Extended on `propose_revoke_engineer_auth`; removed on execution | Timelock proposal for revoking an engineer's per-asset authorization |

#### Expiry consequences — Lifecycle

| Key | If it expires |
| --- | ------------- |
| `HIST` | Maintenance history is lost. `get_collateral_score` and `decay_score` return 0. The asset loses all collateral eligibility. Historical audit trail is permanently destroyed. |
| `SCORE` | Stored accumulated score resets to 0 on next read. `get_collateral_score` falls back to `compute_decay` from history; if `HIST` is still alive the score can be recomputed, but write-back sets 0 as a starting point until `HIST` is processed. |
| `SCHIST` | Score trend history is wiped. `get_score_history` returns an empty vec. Lenders lose visibility into score trajectory but current eligibility is unaffected (it uses `HIST`). |
| `LUPD` | Last-update timestamp is lost. `apply_decay` treats `last_update` as 0 (epoch), causing the full elapsed time since epoch to be used for decay — potentially zeroing the score instantly on the next `decay_score` call. |
| `REGISTRY` | All cross-contract calls to the asset registry panic with `NotInitialized`. `submit_maintenance`, `get_collateral_score`, and `is_collateral_eligible` are all blocked. The contract becomes inoperable until re-initialized (not possible — `initialize` is one-shot). |
| `ENG_REG` | All engineer credential checks panic with `NotInitialized`. `submit_maintenance` is blocked for all assets. |
| `CONFIG` | All operations that read config panic with `NotInitialized`. The contract becomes fully inoperable. |
| `PAUSED` | The pause flag silently expires as `false` (the `unwrap_or(false)` default). A contract that was deliberately paused during an incident will silently unpause, re-enabling all operations without admin action. **Critical safety hazard.** |
| `PADMIN` | The pending admin proposal disappears. The 2-step admin transfer must be restarted from `propose_admin`. No funds or access are lost, but the handover is cancelled. |
| `ENG_AUTH` | The engineer's authorization for that asset is silently revoked. Their next `submit_maintenance` call panics with `EngineerNotAuthorized`. The owner must re-call `authorize_engineer`. |
| `TL_PROP` | The pending timelock proposal is lost. Any queued admin config change is silently cancelled. The admin must re-propose from scratch after the TTL window passes. |
| `FROZEN` | The decommissioned asset no longer appears frozen. `get_collateral_score` falls through to live `compute_decay`, potentially returning a non-zero (or zero) value that diverges from the score captured at decommission. Lending contracts using this score may see inconsistent data. |
| `FRZ_SCR` | The frozen score is lost. Frozen assets return 0 via `get_collateral_score` instead of the value captured at decommission. Any in-progress loans collateralized by this score may be under-collateralized. |
| `HLTH_SNP` | Health snapshot history is wiped. `get_health_snapshots` returns an empty vec. No impact on current score or eligibility. |
| `RVK_TL` | The revoke-engineer timelock proposal expires silently. The revocation is cancelled; `execute_revoke_engineer_auth` will panic with `ProposalNotFound`. The owner must re-propose. |

### 4. Lending Contract

All Lending Contract keys are stored in **persistent** storage. There is no instance storage. Every key is extended on every write using `extend_ttl(TTL_THRESHOLD, TTL_TARGET)`.

| Key Pattern | Extension Policy | Description |
| ----------- | ---------------- | ----------- |
| `Symbol("ADMIN")` | Extended at `initialize` and every admin-transfer function | Admin address for the lending contract |
| `Symbol("TOKEN")` | Extended at `initialize` | Payment token contract address |
| `Symbol("CONFIG")` | Extended at `initialize` and `update_config` | `Config` record (yield BPS, slash BPS) |
| `Symbol("PAUSED")` | Extended on every `pause` and `unpause` | Contract pause flag |
| `Symbol("SL_BAL")` | Extended whenever the slash balance changes (default, slash, withdraw) | Accumulated slash balance in token units |
| `Symbol("SL_BPS")` | Extended on `set_slash_bps` | Slash basis points applied to voucher stakes |
| `Symbol("LOAN_DUR")` | Extended on `set_loan_duration` | Default loan duration in seconds |
| `Symbol("MIN_STK")` | Extended on `set_min_stake` | Minimum vouch stake in token units (stroops) |
| `Symbol("YIELD_BPS")` | Extended on `set_yield_bps` | Yield basis points applied to loan repayments |
| `(Symbol("LOAN"), borrower: Address)` | Extended on `request_loan`, `repay_loan`, `default_loan` | Active `Loan` record for a borrower |
| `(Symbol("BORR"), borrower: Address)` | Extended on `request_loan` and loan closure | Borrower credit history record |
| `(Symbol("VOUCHES"), borrower: Address)` | Extended on `vouch`, `unvouch`, loan closure | `Vec<Vouch>` — all active voucher stakes for a borrower |
| `(Symbol("V_HIST"), voucher: Address)` | Extended on `vouch` and every voucher settlement | `VoucherHistory` — running yield and slash totals |

#### Expiry consequences — Lending Contract

| Key | If it expires |
| --- | ------------- |
| `ADMIN` | All admin-gated functions (`pause`, `unpause`, `set_*`, `withdraw_slash`) panic. The contract becomes permanently un-administrable with no recovery path. **Critical.** |
| `TOKEN` | All token transfer calls (repay, slash, withdraw) panic with `NotInitialized`. No loan can be repaid or defaulted. |
| `CONFIG` | All functions that read config panic. The contract becomes inoperable. |
| `PAUSED` | Same hazard as Lifecycle `PAUSED`: a deliberately paused contract silently unpauses. **Critical safety hazard.** |
| `SL_BAL` | Accumulated slash balance is lost. Previously slashed funds appear to not exist; `withdraw_slash` transfers 0 tokens to the admin. Slashed funds are effectively unrecoverable. |
| `SL_BPS` | Slash rate resets to 0 (default). Future defaults slash 0% of voucher stakes — vouchers bear no risk. |
| `LOAN_DUR` | Loan duration resets to 0, making all future loans immediately overdue. |
| `MIN_STK` | Minimum stake requirement resets to 0. Any vouch amount (including 0) is accepted. |
| `YIELD_BPS` | Yield rate resets to 0. Vouchers earn no yield on repaid loans. |
| `LOAN` | The active loan record is lost. The borrower cannot repay (no record to update) and the admin cannot default (no record to read). The vouched stake is effectively frozen. |
| `BORR` | The borrower credit history is lost. Future loan eligibility decisions lose historical context. No direct operational impact on active loans. |
| `VOUCHES` | All voucher records for the borrower are lost. On default or repayment, no vouchers can be settled, slashed, or rewarded. Voucher funds are unrecoverable. |
| `V_HIST` | The voucher's yield and slash history is lost. `get_voucher_history` returns zeroed totals. No impact on active operations, but historical reporting is destroyed. |

> **Issue #756 — Pause state TTL**: `PAUSED_KEY` is stored in persistent storage. `pause` and `unpause` must call `extend_ttl(&PAUSED_KEY, TTL_THRESHOLD, TTL_TARGET)` after every write so the pause flag cannot silently expire while the contract is paused during an incident response.

## Extension Logic

### Instance Storage

Instance storage holds the admin address and other critical configuration. If it expires, all admin-gated operations (`pause`, `unpause`, `propose_admin`, `accept_admin`, `upgrade`, `add_trusted_issuer`, `remove_trusted_issuer`) will panic with `NotInitialized`.

To prevent this, **every admin-mutating function** calls `env.storage().instance().extend_ttl(518400, 518400)` after its writes. This ensures the instance TTL is refreshed on every admin interaction, keeping it alive as long as the contract is actively administered.

Functions that extend instance TTL in **AssetRegistry**:
- `initialize_admin`
- `propose_admin`
- `accept_admin`
- `pause`
- `unpause`
- `upgrade`

Functions that extend instance TTL in **EngineerRegistry**:
- `initialize_admin`
- `propose_admin`
- `accept_admin`
- `pause`
- `unpause`
- `upgrade`
- `add_trusted_issuer`
- `remove_trusted_issuer`

### Persistent Storage — Pause Flag (Lending Contract)

The Lending Contract stores all data in persistent storage (no instance storage). The `PAUSED` key is extended on every `pause` and `unpause` call:

```rust
env.storage().persistent().extend_ttl(&PAUSED_KEY, TTL_THRESHOLD, TTL_TARGET);
```

Functions that extend `PAUSED_KEY` TTL in **LendingContract**:
- `pause`
- `unpause`

Without this extension, a contract paused during an incident could silently unpause when the persistent entry expires, defeating the safety mechanism.

### Persistent Storage

All `persistent` entries are extended upon every `set` operation using `extend_ttl(518400, 518400)`.

### Manual Extension

Use the Soroban CLI to extend entries if they are near expiration but no write operations are expected:

```bash
stellar contract storage extend --id <CONTRACT_ID> \
  --key '<KEY_XDR>' \
  --durability persistent \
  --ledgers-to-extend 518400
```

## Why Instance TTL Matters

Instance storage is **not** automatically extended on every contract invocation. If the instance TTL expires:

- `get_admin` panics with `NotInitialized`, locking out all admin operations
- Trusted issuer lookups return empty, blocking engineer registration
- The contract becomes unrecoverable without re-deploying

The fix is to call `env.storage().instance().extend_ttl(518400, 518400)` in every function that writes to instance storage, ensuring the TTL is refreshed on every admin interaction.
