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

| Key Pattern | Storage Type | Description |
| ----------- | ------------ | ----------- |
| `(Symbol("HIST"), asset_id: u64)` | Persistent | `Vec<MaintenanceRecord>` of all verified events |
| `(Symbol("SCORE"), asset_id: u64)` | Persistent | Current maintenance health score (0-100) |
| `(Symbol("L_UPD"), asset_id: u64)` | Persistent | Timestamp of the last maintenance or decay event |
| `Symbol("REGISTRY")` | Instance | Linked Asset Registry contract address |
| `Symbol("ENG_REG")` | Instance | Linked Engineer Registry contract address |
| `Symbol("CONFIG")` | Instance | `Config` record (max history, decay rates, etc.) |

### 4. Lending Contract

| Key Pattern | Storage Type | Description |
| ----------- | ------------ | ----------- |
| `Symbol("ADMIN")` | Persistent | Admin address authorized for admin operations |
| `Symbol("TOKEN")` | Persistent | Payment token contract address |
| `Symbol("CONFIG")` | Persistent | `Config` record (yield BPS, slash BPS) |
| `Symbol("PAUSED")` | Persistent | Contract pause flag |
| `Symbol("SL_BAL")` | Persistent | Accumulated slash balance |
| `Symbol("SL_BPS")` | Persistent | Slash basis points |
| `Symbol("LOAN_DUR")` | Persistent | Loan duration in seconds |
| `Symbol("MIN_STK")` | Persistent | Minimum vouch stake |
| `Symbol("YIELD_BPS")` | Persistent | Yield basis points |
| `(Symbol("LOAN"), borrower: Address)` | Persistent | `Loan` record for a borrower |
| `(Symbol("BORR"), borrower: Address)` | Persistent | Borrower credit history record |
| `(Symbol("VOUCHES"), borrower: Address)` | Persistent | `Vec<Vouch>` for a borrower |
| `(Symbol("V_HIST"), voucher: Address)` | Persistent | Voucher history record |

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
