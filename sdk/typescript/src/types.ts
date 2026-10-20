/**
 * @file WireTypes
 * @description The wire event types the SDK hands out to its consumers.
 *
 * Responsibilities:
 * - Mirror `wavecode_wire::EventMsg` so the TypeScript union and the Rust
 *   enum stay one contract.
 *
 * Constraints:
 * - A wire change updates this file in the same change as the Rust enum.
 */

/** The approval kind a gate request carries: exec (command execution) or write. */
export type ApprovalKind = "exec" | "write";

/**
 * The decision line dialect accepted on stdin with `--approvals`.
 *
 * `"deny"` is a bare refusal; `{ deny: "reason" }` carries the reason.
 */
export type ApprovalDecision = "allow" | "always" | "deny" | { deny?: string };

/**
 * Which exit of the dispatch pipeline produced a tool result: `is_error`
 * says the call did not succeed, this says *who* stopped it — the tool
 * itself, the model's caller, the policy, a hook, or the user.
 */
export type ToolOutcome =
  | "executed"
  | "denied"
  | "refused"
  | "hook_blocked"
  | "surface_blocked"
  | "interrupted"
  | "duplicate"
  | "answered"
  | "ask_unavailable"
  | "missing";

/** One wire event, correlated with a submission by `id`. */
export type WireEvent = { id: string } & WireEventPayload;

/**
 * The payload of one wire event: the `type` tag plus that variant's
 * fields.
 *
 * The variant set mirrors `wavecode_wire::EventMsg` (internal tag
 * `type`, snake_case fields). A field the sender may omit is optional
 * here; every other field is always present on the wire.
 */
export type WireEventPayload =
  | {
      type: "turn_started";
      /** Wire model name for this turn; absent from older senders. */
      model?: string;
    }
  /** Incremental assistant text. */
  | { type: "agent_message_delta"; text: string }
  /** Incremental extended-thinking text (display only). */
  | { type: "agent_thinking_delta"; text: string }
  /** The assistant message completed, carrying its full text. */
  | { type: "agent_message_complete"; text: string }
  /** A tool call started. */
  | { type: "tool_call_begin"; call_id: string; name: string; input: unknown }
  /** A tool call finished. */
  | {
      type: "tool_call_end";
      call_id: string;
      is_error: boolean;
      /** Bounded head of the tool result; absent from older senders. */
      output?: { text: string; truncated: boolean };
      /** Which dispatch exit produced the result; absent from older senders. */
      outcome?: ToolOutcome;
      /** Wall-clock milliseconds of the tool body; 0 when the body never ran. */
      duration_ms?: number;
    }
  /** An approval request is parked and needs a user decision. */
  | {
      type: "approval_requested";
      call_id: string;
      kind: ApprovalKind;
      detail: string;
    }
  /** An interactive question is parked and needs a user answer. */
  | {
      type: "question_requested";
      call_id: string;
      question: string;
      options: string[];
    }
  /** Token usage settled after a sample. */
  | {
      type: "token_count";
// nosemgrep: codacy.yaml.security.hard-coded-tokens
      input_tokens: number;
// nosemgrep: codacy.yaml.security.hard-coded-tokens
      output_tokens: number;
      cache_read_tokens?: number;
      cache_creation_tokens?: number;
      context_window?: number;
      context_used?: number;
    }
  /** Compaction started. */
  | { type: "compact_started"; trigger: string }
  /** Compaction completed. */
  | { type: "compact_completed"; summary_tokens: number }
  /** A rewind landed: the dropped turns are gone from the history. */
  | { type: "history_rewound"; turns: number }
  /** A reviewed plan was proposed and awaits approval. */
  | { type: "plan_proposed"; text: string }
  /** A reviewed plan was approved. */
  | { type: "plan_approved" }
  /** A durable goal was set. */
  | { type: "goal_set"; objective: string }
  /** A durable goal was completed. */
  | { type: "goal_completed" }
  /** Non-fatal condition worth surfacing. */
  | { type: "warning"; message: string }
  /** Fatal condition for this run. */
  | {
      type: "error";
      message: string;
      recoverable: boolean;
      /** `domain.reason` class (`provider.timeout`, `hook.blocked`, …). */
      code?: string;
    }
  /** The run terminated; always the last event of a submission. */
  | { type: "turn_completed"; interrupted: boolean };

/**
 * The leading control line of a `--json` run (`exec --approvals` may also
 * answer its approvals): identifies the journaled session and the command
 * that resumes it.
 */
export interface MetaLine {
  meta: "session";
  session_id: string;
  version: string;
  resume: string;
}

/** Anything the event iterator can yield: the meta line or a wire event. */
export type ExecEvent = MetaLine | WireEvent;

/**
 * Narrow a stream event to the leading meta line.
 *
 * @param event - One event yielded by the session's iterator.
 * @returns True when the event is the meta line identifying the session.
 */
export function isMetaLine(event: ExecEvent): event is MetaLine {
  return (event as { meta?: unknown }).meta === "session";
}
