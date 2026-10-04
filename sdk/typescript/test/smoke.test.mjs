/**
 * End-to-end smoke tests against the real wavecode binary. Skipped when
 * no binary is found (`WAVECODE_BIN` or the repo's target/debug build)
 * and when no live provider is opted in: both tests drive real agent
 * turns, which need a configured model. Set WAVECODE_SDK_LIVE=1 to run
 * them (the same opt-in convention as the benchmark LIVE=1 tier); they
 * exercise the SDK surface and pin the wire shapes the types mirror —
 * a Rust wire change that breaks them is an SDK breaking change.
 */

import test from "node:test";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { execSession } from "../dist/index.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const candidates = [
  process.env.WAVECODE_BIN,
  path.join(here, "../../../target/debug/wavecode.exe"),
  path.join(here, "../../../target/debug/wavecode"),
].filter(Boolean);

const bin = candidates.find((p) => existsSync(p));

// No binary and no live provider needed: the decision validation runs
// before any wire I/O, so a never-spawnable session exercises the
// fail-closed path directly.
const neverSpawnable = "wavecode-sdk-test-no-such-binary";

test("answerApproval rejects unknown string decisions fail-closed", () => {
  const session = execSession({ bin: neverSpawnable, prompt: "x", approvals: true });
  assert.throws(
    () => session.answerApproval("call-1", "bogus"),
    /unknown approval decision/,
  );
});

test("answerApproval accepts wire decisions and deny objects without validation errors", () => {
  const session = execSession({ bin: neverSpawnable, prompt: "x", approvals: true });
  // The child never spawns (ENOENT surfaces as an error event later), so
  // these only prove decisionToken accepts every documented decision.
  assert.doesNotThrow(() => session.answerApproval("call-1", "allow"));
  assert.doesNotThrow(() => session.answerApproval("call-1", "always"));
  assert.doesNotThrow(() => session.answerApproval("call-1", "deny"));
  assert.doesNotThrow(() => session.answerApproval("call-1", { deny: "policy" }));
});

test("answerApproval refuses sessions without approvals: true", () => {
  const session = execSession({ bin: neverSpawnable, prompt: "x" });
  assert.throws(
    () => session.answerApproval("call-1", "allow"),
    /approvals: true/,
  );
});

test("observer run streams meta, deltas, and a clean completion", { timeout: 120_000 }, async (t) => {
  if (!bin) {
    t.skip("no wavecode binary found");
    return;
  }
  if (!process.env.WAVECODE_SDK_LIVE) {
    t.skip("live provider turns are opt-in; set WAVECODE_SDK_LIVE=1");
    return;
  }
  const session = execSession({
    bin,
    prompt: "Reply with exactly: ok",
  });
  const types = [];
  let meta = null;
  for await (const event of session.events) {
    if (event.meta === "session") {
      meta = event;
      continue;
    }
    types.push(event.type);
    if (event.type === "turn_completed") {
      break;
    }
  }
  const result = await session.wait();
  // The meta line ships first and names a resumable session.
  if (!meta) {
    throw new Error("missing session meta line");
  }
  if (!meta.session_id || !meta.resume.includes(meta.session_id)) {
    throw new Error(`meta line incomplete: ${JSON.stringify(meta)}`);
  }
  if (!types.includes("turn_started") || !types.includes("agent_message_delta")) {
    throw new Error(`unexpected event sequence: ${types.join(",")}`);
  }
  if (result.code !== 0) {
    throw new Error(`exit ${result.code}`);
  }
});

test("approval flow answers a parked request through the SDK", { timeout: 180_000 }, async (t) => {
  if (!bin) {
    t.skip("no wavecode binary found");
    return;
  }
  if (!process.env.WAVECODE_SDK_LIVE) {
    t.skip("live provider turns are opt-in; set WAVECODE_SDK_LIVE=1");
    return;
  }
  const session = execSession({
    bin,
    prompt: "You must use the shell tool to run exactly: echo sdk-approval-ok",
    approvals: true,
    permissionMode: "auto",
  });
  let answered = false;
  let sawToolEnd = false;
  for await (const event of session.events) {
    if (event.type === "approval_requested") {
      // Decide programmatically — the whole point of the channel.
      session.answerApproval(event.call_id, "allow");
      answered = true;
    } else if (event.type === "tool_call_end") {
      sawToolEnd = true;
    } else if (event.type === "turn_completed") {
      break;
    }
  }
  const result = await session.wait();
  if (!answered) {
    throw new Error("no approval was requested; the prompt did not trigger a tool call");
  }
  if (!sawToolEnd) {
    throw new Error("the approved tool call never completed");
  }
  if (result.code !== 0) {
    throw new Error(`exit ${result.code}`);
  }
});
