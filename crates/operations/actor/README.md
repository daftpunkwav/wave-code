# crates/operations/actor/ — serial session driver behind one client handle

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; depends on `runtime-runner`, `runtime-child`, `state-store`, `state-checkpoint`, `safety-gate`, `wavecode-wire`, `wavecode-config`, `infrastructure-base` |
| `src/lib.rs` | Crate root; re-exports `SessionActor`, `ActorClient`, the session contract, durability, and `StatusQueries` |
| `src/actor.rs` | `SessionActor`: serial turn driver — pending queue for inputs/compacts, immediate control routing, rewind, lifecycle hooks, optional durable checkpoints |
| `src/client.rs` | `ActorClient`: in-process handle — `submit`, the event stream, the read-only `EventTap`, inbox steering (`steer`/`inject`/`cancel_inbox`), `SubmitError` |
| `src/contract.rs` | Vocabulary shared with the gateway: `AssembleOptions`, `SessionError`, `DEFAULT_IDENTITY`, and the `SessionSurface` trait RPC servers consume |
| `src/durable.rs` | Persist-then-act checkpoints: `DurabilityConfig`, `CheckpointSink`, `persist_checkpoint`, `list_resume_labels`, `turn_label` |
| `src/status.rs` | `StatusQueries` trait: on-demand plan/goal/snapshot read views for frontend slash commands |

The actor serializes user turns while routing control operations
(interrupt, approvals, shutdown) immediately; a full pending queue
rejects explicitly instead of parking an interrupt behind it. It never
names concrete capabilities — everything runs through the generic
`TurnDriver` seam plus the gates and interrupt handle supplied at
spawn. The gateway consumes sessions only through `SessionSurface`,
so this contract is the crate's real public interface.
