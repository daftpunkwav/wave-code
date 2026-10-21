/**
 * A session that cannot start.
 *
 * A failed spawn has to reach the caller as the final error event of
 * the event stream: a `for await` consumer must see that event and
 * then the end of the stream, not an iterator rejection that hides it.
 * The exit promise still rejects for the callers of `wait()`.
 */

import test from "node:test";
import assert from "node:assert/strict";
import { execSession } from "../dist/index.js";

/** A binary that is not on the PATH, so the spawn fails with ENOENT. */
const MISSING_BIN = "wavecode-binary-that-does-not-exist";

test("a spawn failure arrives as a final error event", async () => {
  const session = execSession({ prompt: "hello", bin: MISSING_BIN });

  const events = [];
  for await (const event of session.events) {
    events.push(event);
  }

  assert.equal(events.length, 1, "the failure arrives as one final event");
  assert.equal(events[0].type, "error");
  assert.match(events[0].message, /failed to spawn/);
  assert.equal(events[0].recoverable, false);
});

test("wait() rejects when the spawn fails", async () => {
  const session = execSession({ prompt: "hello", bin: MISSING_BIN });
  await assert.rejects(() => session.wait(), /ENOENT/);
});
