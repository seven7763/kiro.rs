# Kiro proxy reference audit

Date: 2026-05-31

This note records a first-pass comparison between this repository and several public Kiro reverse proxy / `kiro.rs` derivative projects. No source code changes are included here.

## Reference Projects Reviewed

| Project | Local checkout | Commit | Useful signal |
|---|---:|---:|---|
| Foxfishc/kiro.rs | `/tmp/kiro_refs/kiro.rs` | `8331585` | Credential-scoped prompt cache tracker, CLI endpoint body/header handling, per-request `profileArn` injection |
| TsinHzl/kiro2cc-proxy-local | `/tmp/kiro_refs/kiro2cc-proxy-local` | `66075bb` | Close upstream ancestry for Rust conversion, basic proxy behavior |
| htesd/kirorsfornewapi | `/tmp/kiro_refs/kirorsfornewapi` | `02111ab` | Alternative Rust fork with context management and perceived cache behavior |
| jwadow/kiro-gateway | `/tmp/kiro_refs/kiro-gateway` | `a5292ca` | Account manager design: sticky account, circuit breaker, exponential backoff, state persistence |
| bestK/kiro2cc | `/tmp/kiro_refs/kiro2cc` | `f3dabe9` | Minimal Go proxy; useful as a simple baseline for request conversion |
| d-kuro/kirocc | `/tmp/kiro_refs/kirocc` | `6aa5310` | CLI-oriented proxy reference |
| petehsu/KiroProxy | `/tmp/kiro_refs/KiroProxy` | `9a91b9b` | Python proxy / capture utilities |
| Finesssee/ProxyPilot | `/tmp/kiro_refs/ProxyPilot` | `a1ce124` | Broader multi-provider translator patterns and usage mapping ideas |

## Key Findings

### P0: Prompt cache accounting and conversation reuse happen before real credential selection

Current local flow computes prompt cache decisions in the Anthropic handler before `KiroProvider` selects the actual credential:

- [src/anthropic/handlers.rs](/Users/hei/ai滲透/kiro/kiro-rs-dev/src/anthropic/handlers.rs:88) calls `cache.compute(GLOBAL_ACCOUNT, &profile)`.
- [src/anthropic/handlers.rs](/Users/hei/ai滲透/kiro/kiro-rs-dev/src/anthropic/handlers.rs:120) writes cache entries back to `GLOBAL_ACCOUNT`.
- The actual credential is selected later in provider retry logic.

Risk:

- Cache usage can report hits from a global bucket even when the next upstream request is routed to a different Kiro account.
- This is especially wrong under `balanced` mode and during retry/failover.
- It also makes Admin cache hit metrics look better than actual upstream reuse.

Reference:

- `Foxfishc/kiro.rs` computes and updates cache by `credential_id` in `CacheTracker::compute(credential_id, profile)` / `update(credential_id, profile)`.
- It resolves cache usage after the provider has returned the credential id, including websearch paths.

Suggested fix:

1. Move cache usage resolution to the provider result path, or return selected `credential_id` from provider calls.
2. Compute cache usage against that credential bucket.
3. Update that credential bucket only after upstream success.
4. Keep the handler building a `CacheProfile`, but defer `compute/update` until the credential is known.

### P0: Prompt cache conversation id reuse is cross-account

Current `PromptCache` stores `conversation_by_fingerprint` globally:

- [src/anthropic/prompt_cache.rs](/Users/hei/ai滲透/kiro/kiro-rs-dev/src/anthropic/prompt_cache.rs:129) documents it as cross-account shared.
- [src/anthropic/prompt_cache.rs](/Users/hei/ai滲透/kiro/kiro-rs-dev/src/anthropic/prompt_cache.rs:417) inserts by fingerprint only.
- [src/anthropic/prompt_cache.rs](/Users/hei/ai滲透/kiro/kiro-rs-dev/src/anthropic/prompt_cache.rs:432) looks up by fingerprint only.

Risk:

- Account A can create a conversation id for a prefix.
- A later balanced/failover request can reuse that same conversation id with account B.
- That can poison sticky routing, reduce real cache hit rate, or cause upstream session/account mismatch behavior.

Suggested fix:

- Make conversation reuse credential-scoped: `account -> fingerprint -> conversation_id`.
- Better: remove forced conversation reuse from the handler and let provider sticky routing bind `conversation_id -> credential_id` only after successful upstream calls.

### P0: Per-credential concurrency permit is stored in shared credential state

Current `CredentialEntry` has a single `concurrency_permit: Option<OwnedSemaphorePermit>`:

- [src/kiro/token_manager.rs](/Users/hei/ai滲透/kiro/kiro-rs-dev/src/kiro/token_manager.rs:446)
- Sticky path overwrites it at [src/kiro/token_manager.rs](/Users/hei/ai滲透/kiro/kiro-rs-dev/src/kiro/token_manager.rs:1052).
- Normal selection overwrites it at [src/kiro/token_manager.rs](/Users/hei/ai滲透/kiro/kiro-rs-dev/src/kiro/token_manager.rs:1141).

Risk:

- If two requests select the same credential concurrently, the second permit assignment drops the first permit.
- Dropping an `OwnedSemaphorePermit` releases the slot early.
- `max_inflight_per_credential` can therefore be bypassed under concurrency.
- On acquire failure the code logs "降级为不限并发" and still uses the credential, which also defeats the protection.

Suggested fix:

- Move `OwnedSemaphorePermit` into `CallContext` so each request owns its permit until release.
- If a credential semaphore is full, skip that credential and select another live candidate. Only fall back to an at-capacity credential if every otherwise eligible credential is at capacity and the configured behavior explicitly allows it.

### P1: Prompt cache TTL is extended on every local hit

Current code refreshes `expires_at` when a local cache entry is hit:

- [src/anthropic/prompt_cache.rs](/Users/hei/ai滲透/kiro/kiro-rs-dev/src/anthropic/prompt_cache.rs:357)

Risk:

- Anthropic-style ephemeral prompt cache TTL is normally measured from creation/write, not extended indefinitely by reads.
- Local metrics can show cache hits long after upstream would have expired the prefix.

Reference:

- `Foxfishc/kiro.rs` explicitly avoids refreshing `expires_at` on hit, noting that otherwise local cache read numbers become inflated versus upstream behavior.

Suggested fix:

- Do not refresh expiry on read.
- On update, avoid extending expiry for an existing fingerprint unless upstream semantics are confirmed to renew it.

### P1: Cache profile fingerprint omits request prelude fields

Current local fingerprinting focuses on content blocks. It does not appear to include stable request-level fields such as `model` or `tool_choice`.

Risk:

- The same prefix under different model/tool-choice semantics can collide in local cache accounting.
- This can overstate cache hits and accidentally reuse a conversation id across incompatible request shapes.

Reference:

- `Foxfishc/kiro.rs` includes a canonical request prelude containing `model` and `tool_choice` in the prefix hash.

Suggested fix:

- Include canonicalized request prelude fields in the cache hasher before content blocks.
- At minimum include `model`, `tool_choice`, and any other field that changes upstream prefix-cache identity.

### P1: Balanced scheduling has good recent fixes, but lacks explicit per-request exclusion

Local code already has several strong ideas:

- Inflight-aware balanced selection.
- Cooldown skipping.
- Fallback path that does not extend cooldown.
- Random tiebreakers.
- Sticky conversation routing.

The remaining gap is that retry attempts rely mostly on cooldown state rather than an explicit `exclude_already_tried` set.

Reference:

- `kiro-gateway` passes `exclude_accounts` through its failover loop. Its account manager also keeps a global sticky index updated only on success, and it applies circuit breaker backoff to unhealthy accounts.

Risk:

- Some non-cooldown retry paths can revisit a just-tried credential within the same request.
- The behavior is harder to reason about than a retry loop that explicitly says "do not retry this account in this request unless all accounts have been exhausted."

Suggested fix:

- Add per-request `tried_credential_ids` to provider retry loops.
- Pass exclusions into `acquire_context`.
- Reset exclusions only when every eligible credential has been tried.

### P1: Account health is process-local and only partially persistent

Local stats persist success and transient counters, but cooldown and last transient instants are process-local. `kiro-gateway` persists more account manager state and uses exponential backoff/circuit breaker behavior.

Risk:

- Restart clears cooldowns and can immediately hit a storming upstream again.
- A large account pool can relearn the same bad accounts after restart.

Suggested fix:

- Persist a coarse account health state: failure count, last failure timestamp, disabled reason, cooldown reason, and cooldown expiry as wall-clock RFC3339.
- On startup, restore only entries whose cooldown expiry is still in the future.

### P2: Structure drift between endpoints and admin/runtime paths

Local project has accumulated endpoint, admin, metrics, prompt cache, and retry runtime behavior in a small number of large files.

Risk:

- Cross-cutting changes are easy to apply in one path and miss another.
- Prompt cache has duplicate handler call sites for streaming and non-streaming.
- Provider retry, token refresh, cooldown, sticky routing, and concurrency are all concentrated in `token_manager.rs` / `provider.rs`.

Suggested refactor sequence:

1. Extract cache profile/accounting into a provider-visible module that can consume `credential_id`.
2. Extract credential selection into a strategy object or smaller module.
3. Keep token refresh separate from request scheduling.
4. Add a compact provider result type carrying `response`, `credential_id`, fallback/wait flags, and conversation id.

## Suggested Implementation Order

1. Fix per-request semaphore permit ownership.
2. Stop cross-account forced conversation id reuse.
3. Move prompt cache compute/update from handler pre-selection to provider post-selection by real `credential_id`.
4. Adjust prompt cache TTL behavior to not refresh on read.
5. Add request prelude to prompt cache fingerprints.
6. Add per-request credential exclusion in retry loops.
7. Persist cooldown/account health state.
8. Refactor modules only after behavior is covered by tests.

## Tests To Add

- Balanced mode: concurrent acquires respect `max_inflight_per_credential`.
- Prompt cache: account A update does not produce read hit for account B.
- Prompt cache: account A conversation id is never reused for account B.
- Prompt cache: hit does not extend TTL.
- Retry loop: same request does not retry the same credential while another eligible credential exists.
- Handler/provider integration: reported cache usage uses the final credential id, including after retry failover.

