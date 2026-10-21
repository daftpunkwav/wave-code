/**
 * @file Index
 * @description The package entry point: the SDK's public export surface.
 *
 * Responsibilities:
 * - Re-export the session API and the wire types that API hands out.
 *
 * Constraints:
 * - This module re-exports only: it holds no behaviour of its own.
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
  ToolOutcome,
  WireEvent,
  WireEventPayload,
} from "./types.js";
