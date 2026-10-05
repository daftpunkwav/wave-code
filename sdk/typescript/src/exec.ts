/**
 * Spawn `wavecode exec --json` as a child process and expose the JSONL
 * event stream as a typed async iterator. With `approvals: true` the
 * session also answers parked approvals through the `answerApproval`
 * handle (the wire's stdin dialect); without it the run stays
 * fail-closed: approvals deny, exactly like plain exec.
 */

import { spawn, type ChildProcessByStdio } from "node:child_process";
import type { Writable } from "node:stream";
import readline from "node:readline";
import type { ApprovalDecision, ExecEvent } from "./types.js";

export { isMetaLine } from "./types.js";
export type { ApprovalDecision, ExecEvent, MetaLine, WireEvent } from "./types.js";

export interface ExecOptions {
  /** The user prompt for the single turn. */
  prompt: string;
  /**
   * Opt in to answering parked approvals through `answerApproval`.
   * Default false: approvals deny (fail-closed), matching plain exec.
   */
  approvals?: boolean;
  /** Working directory for the agent session (default: process cwd). */
  cwd?: string;
  /** Model override (`--model`). */
  model?: string;
  /** Permission mode override (`--permission-mode`: plan/auto/wave). */
  permissionMode?: string;
  /** Config file override (`--config`). */
  configPath?: string;
  /**
   * wavecode binary to run. Default: `$WAVECODE_BIN`, else `wavecode`
   * resolved from PATH (add `.exe` resolution happens via spawn on
   * Windows PATHEXT through the shell-less lookup of the exact name —
   * prefer setting WAVECODE_BIN on Windows when the binary is not
   * named `wavecode.exe`).
   */
  bin?: string;
  /** Extra environment for the child (merged over `process.env`). */
  env?: Record<string, string>;
}

export interface ExecResult {
  /** Process exit code (0 completed, 130 interrupted, 1 failed). */
  code: number;
  /** The turn ended at an interrupt checkpoint. */
  interrupted: boolean;
}

export interface ExecSession {
  /** Async iterator over the meta line plus every wire event. */
  events: AsyncIterable<ExecEvent>;
  /**
   * Answer one parked approval (`approvals: true` only). The decision
   * maps onto the wire stdin dialect: `"allow"`, `"always"`, or
   * `{ deny: "reason" }` / `"deny"`.
   */
  // The parameter name documents the wire protocol; the base rule
  // cannot see that an interface member list is a type position.
  // eslint-disable-next-line no-unused-vars -- documentation name, no body exists
  answerApproval(callId: string, decision: ApprovalDecision): void;
  /** Resolve when the child exits; also the iterator end. */
  wait(): Promise<ExecResult>;
  /** SIGINT-equivalent: interrupt the running turn. */
  interrupt(): void;
}

/** String decisions the wire stdin dialect accepts verbatim. */
const WIRE_DECISIONS: readonly string[] = ["allow", "always", "deny"];

type WireDecision = "allow" | "always" | "deny";

function isWireDecision(value: string): value is WireDecision {
  return WIRE_DECISIONS.includes(value);
}

/** Internal: the decision token accepted by the wire stdin dialect. */
function decisionToken(decision: ApprovalDecision): string {
  if (typeof decision === "string") {
    // Runtime validation is deliberate: the TypeScript union cannot be
    // enforced when SDK consumers call from plain JavaScript, so unknown
    // strings fail closed here instead of leaking into the wire dialect.
    if (isWireDecision(decision)) {
      return decision;
    }
    throw new Error(`unknown approval decision: ${String(decision)}`);
  }
  const reason = decision.deny?.trim();
  return reason ? `deny:${reason}` : "deny";
}

export function execSession(options: ExecOptions): ExecSession {
  const bin = options.bin ?? process.env.WAVECODE_BIN ?? "wavecode";
  const args = ["exec", "--json"];
  if (options.approvals) {
    args.push("--approvals");
  }
  if (options.model) {
    args.push("--model", options.model);
  }
  if (options.permissionMode) {
    args.push("--permission-mode", options.permissionMode);
  }
  if (options.configPath) {
    args.push("--config", options.configPath);
  }
  args.push(options.prompt);

  // stdout and stdin are pipes (event stream and approval answers);
  // stderr inherits so human-side progress stays visible by default.
  const child: ChildProcessByStdio<Writable, import("node:stream").Readable, null> =
    spawn(bin, args, {
      cwd: options.cwd,
      env: options.env ? { ...process.env, ...options.env } : process.env,
      stdio: ["pipe", "pipe", "inherit"],
      windowsHide: true,
    });

  const eventsQueue: ExecEvent[] = [];
  let wake: (() => void) | null = null;
  let closed = false;
  // The parameter name documents what a rejection receives; the base
  // rule cannot see that this is a type position, not an unused binding.
  // eslint-disable-next-line no-unused-vars -- type-position param name
  let fail: ((error: Error) => void) | null = null;

  // This SDK file is not a Qwik component; the framework-scoped rule
  // misfires on a plain closure.
  // biome-ignore lint/correctness/useQwikValidLexicalScope: not Qwik code
  const push = (event: ExecEvent) => {
    eventsQueue.push(event);
    wake?.();
    wake = null;
  };

  const stdout = child.stdout;
  // The Readable type claims non-null, but child.stdout is null when
  // spawn fails (ENOENT); the error handler below surfaces that as a
  // final event and this guard keeps the readline setup from throwing.
  // eslint-disable-next-line @typescript-eslint/no-unnecessary-condition -- runtime null despite the type
  if (stdout) {
    const lines = readline.createInterface({ input: stdout, crlfDelay: Infinity });
    lines.on("line", (line) => {
      const trimmed = line.trim();
      if (!trimmed) {
        return;
      }
      try {
        push(JSON.parse(trimmed) as ExecEvent);
      } catch {
        push({
          type: "error",
          message: `unparsable stdout line: ${trimmed.slice(0, 200)}`,
          recoverable: false,
          id: "",
        });
      }
    });
  }
  child.on("error", (error) => {
    // Spawn failure (ENOENT, …): surface as a final event, not a hang.
    fail?.(error);
    fail = null;
    push({
      type: "error",
      message: `failed to spawn ${bin}: ${error.message}`,
      recoverable: false,
      id: "",
    });
    closed = true;
    wake?.();
    wake = null;
  });
  child.on("close", () => {
    closed = true;
    wake?.();
    wake = null;
  });

  const events: AsyncIterable<ExecEvent> = {
    [Symbol.asyncIterator]() {
      return {
        async next(): Promise<IteratorResult<ExecEvent>> {
          for (;;) {
            const event = eventsQueue.shift();
            if (event) {
              return { value: event, done: false };
            }
            if (closed) {
              return { value: undefined, done: true };
            }
            await new Promise<void>((resolve, reject) => {
              wake = resolve;
              fail = reject;
            });
          }
        },
      };
    },
  };

  return {
    events,
    answerApproval(callId: string, decision: ApprovalDecision): void {
      if (!options.approvals) {
        throw new Error(
          "answerApproval requires `approvals: true` — without it the run is fail-closed",
        );
      }
      child.stdin.write(`${callId} ${decisionToken(decision)}\n`);
    },
    wait(): Promise<ExecResult> {
      return new Promise((resolve, reject) => {
        child.on("error", reject);
        child.on("close", (code) => {
          resolve({
            code: code ?? 1,
            interrupted: code === 130,
          });
        });
      });
    },
    interrupt(): void {
      child.kill("SIGINT");
    },
  };
}
