/**
 * @wavecode/sdk — drive the WaveCode coding agent from JavaScript.
 *
 * Minimal surface: `execSession` spawns `wavecode exec --json`, exposes
 * the typed event stream, and (opt-in) answers approvals.
 */

export { execSession } from "./exec.js";
export type { ExecOptions, ExecResult, ExecSession } from "./exec.js";
export {
  isMetaLine,
} from "./types.js";
export type {
  ApprovalDecision,
  ApprovalKind,
  ExecEvent,
  MetaLine,
  WireEvent,
  WireEventPayload,
} from "./types.js";
