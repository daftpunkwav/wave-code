/**
 * Wire event types: a hand-maintained single source mirroring the Rust
 * `wavecode_wire::EventMsg` serde serialization (internal tag `type`,
 * snake_case names and fields). Locked by the SDK smoke test against the
 * real binary; when the Rust enum changes, update this union in the same
 * commit as the wire change.
 */

/** The approval kind a gate request carries: exec (command execution) or write. */
export type ApprovalKind = "exec" | "write";

/** The decision line dialect accepted on stdin with `--approvals`. */
export type ApprovalDecision = "allow" | "always" | { deny?: string };

/** One wire event, correlated with a submission by `id`. */
export type WireEvent = { id: string } & WireEventPayload;

export type WireEventPayload =
  | { type: "turn_started" }
  | { type: "agent_message_delta"; text: string }
  | { type: "agent_thinking_delta"; text: string }
  | { type: "agent_message_complete"; text: string }
  | { type: "tool_call_begin"; call_id: string; name: string; input: unknown }
  | {
      type: "tool_call_end";
      call_id: string;
      is_error: boolean;
      /** Bounded head of the tool result; absent from older senders. */
      output?: { text: string; truncated: boolean };
    }
  | {
      type: "approval_requested";
      call_id: string;
      kind: ApprovalKind;
      detail: string;
    }
  | {
      type: "question_requested";
      call_id: string;
      question: string;
      options: string[];
    }
  | {
      type: "token_count";
      input_tokens: number;
      output_tokens: number;
      cache_read_tokens?: number;
      cache_creation_tokens?: number;
      context_window?: number;
      context_used?: number;
    }
  | { type: "compact_started"; trigger: string }
  | { type: "compact_completed"; summary_tokens: number }
  | { type: "history_rewound"; turns: number }
  | { type: "plan_proposed"; text: string }
  | { type: "plan_approved" }
  | { type: "goal_set"; objective: string }
  | { type: "goal_completed" }
  | { type: "warning"; message: string }
  | {
      type: "error";
      message: string;
      recoverable: boolean;
      /** `domain.reason` class (`provider.timeout`, `hook.blocked`, …). */
      code?: string;
    }
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

export function isMetaLine(event: ExecEvent): event is MetaLine {
  return (event as { meta?: unknown }).meta === "session";
}
