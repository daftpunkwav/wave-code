/**
 * @file ExecSession
 * @description Spawn `wavecode exec --json` and expose its JSONL event
 *   stream as a typed async iterator.
 *
 * Responsibilities:
 * - Spawn the child process and parse its JSONL stdout into typed events.
 * - Answer parked approvals through the wire's stdin dialect.
 * - Surface the child's exit code and whether the turn was interrupted.
 *
 * Constraints:
 * - This module must not depend on the Rust workspace: it drives the
 *   `wavecode` binary over its wire only.
 * - Without `approvals: true` the run stays fail-closed: approvals deny,
 *   exactly like plain exec.
 */

import { spawn, type ChildProcessByStdio } from "node:child_process";
import type { Writable } from "node:stream";
import readline from "node:readline";
import type { ApprovalDecision, ExecEvent } from "./types.js";

export { isMetaLine } from "./types.js";
export type { ApprovalDecision, ExecEvent, MetaLine, WireEvent } from "./types.js";

/** The run's prompt, binary, and surface options. */
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
   * resolved from PATH. The lookup is shell-less: it tries the exact
   * name, then the executable extensions the runtime knows (`.com`,
   * `.exe`), and it never consults `PATHEXT` — a `.cmd` launcher is
   * not found, so set WAVECODE_BIN on Windows when the binary is not
   * named `wavecode.exe`).
   */
  bin?: string;
  /** Extra environment for the child (merged over `process.env`). */
  env?: Record<string, string>;
}

/** How the child process ended. */
export interface ExecResult {
  /** Process exit code (0 completed, 130 interrupted, 1 failed). */
  code: number;
  /** The turn ended at an interrupt checkpoint. */
  interrupted: boolean;
}

/** The live run: its event stream, its approval handle, and its exit. */
export interface ExecSession {
  /** Async iterator over the meta line plus every wire event. */
  events: AsyncIterable<ExecEvent>;
  /**
   * Answer one parked approval (`approvals: true` only). The decision
   * maps onto the wire stdin dialect: `"allow"`, `"always"`, or
   * `{ deny: "reason" }` / `"deny"`.
   *
   * @throws When the run was started without `approvals: true` — that run
   * is fail-closed and parks no decision for the caller to make — or when
   * the decision is not one the wire dialect accepts.
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

/**
 * Spawn `wavecode exec --json` and expose its JSONL event stream.
 *
 * @param options - The prompt, binary, and surface options of the run.
 * @returns The live session: its event iterator, its approval handle,
 * and the promise that settles when the child exits.
 */
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
  // The exit promise is created once, here: a `wait()` call made after
  // the child already closed must still settle, and a promise built per
  // call would wait for an event that has already fired.
  const exit = new Promise<ExecResult>((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code) => {
      resolve({
        code: code ?? 1,
        interrupted: code === 130,
      });
    });
  });
  // The promise is created before anyone can await it, so a spawn
  // failure would surface as an unhandled rejection for a caller that
  // only reads the event stream. The same failure reaches that caller
  // as a final event, so the rejection itself is left unhandled here.
  exit.catch(() => undefined);
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
    // Spawn failure (ENOENT, —): surface as a final event, not a hang.
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
      return exit;
    },
    interrupt(): void {
      child.kill("SIGINT");
    },
  };
}
