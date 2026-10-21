/**
 * The exit promise of a session.
 *
 * `wait()` has to settle for a caller that read the event stream to the
 * end first: the child has already closed by then, so a promise built
 * per call would wait for an event that has already fired and never
 * resolve.
 */

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { execSession } from "../dist/index.js";

/**
 * Race a promise against a timeout, and clear the timer once the race
 * settles: a pending timer keeps the event loop alive and holds the
 * test process open for the whole timeout window.
 */
async function settleWithin(promise, ms) {
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(() => reject(new Error("wait() did not settle")), ms);
  });
  try {
    return await Promise.race([promise, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

test("wait() settles after the event stream ended", async () => {
  // The SDK spawns `wavecode exec --json <prompt>`, so a script named
  // `exec` in the working directory stands in for the binary: node runs
  // it, and it exits at once without writing to stdout.
  const dir = mkdtempSync(path.join(tmpdir(), "wavecode-sdk-"));
  writeFileSync(path.join(dir, "exec"), "process.exit(0);\n");
  try {
    const session = execSession({
      prompt: "hello",
      bin: process.execPath,
      cwd: dir,
    });

    let events = 0;
    for await (const _event of session.events) {
      events += 1;
    }
    assert.equal(events, 0, "the stand-in binary writes no events");

    // The child has closed by now, so a promise built per call would
    // wait for an event that already fired and never settle.
    const result = await settleWithin(session.wait(), 5_000);
    assert.equal(result.code, 0);
    assert.equal(result.interrupted, false);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("wait() settles before the event stream ended too", async () => {
  const dir = mkdtempSync(path.join(tmpdir(), "wavecode-sdk-"));
  writeFileSync(path.join(dir, "exec"), "process.exit(0);\n");
  try {
    const session = execSession({
      prompt: "hello",
      bin: process.execPath,
      cwd: dir,
    });
    const result = await settleWithin(session.wait(), 5_000);
    assert.equal(result.code, 0);
    assert.equal(result.interrupted, false);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
