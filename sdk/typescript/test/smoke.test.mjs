/**
 * End-to-end smoke tests against the real wavecode binary. Skipped when
 * no binary is found (`WAVECODE_BIN` or the repo's target/debug build);
 * these exercise the SDK surface and pin the wire shapes the types
 * mirror — a Rust wire change that breaks them is an SDK breaking change.
 */

import test from "node:test";
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

test("observer run streams meta, deltas, and a clean completion", { timeout: 120_000 }, async (t) => {
  if (!bin) {
    t.skip("no wavecode binary found");
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
