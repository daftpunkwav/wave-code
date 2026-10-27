/**
 * The exec argv contract, pinned without spawning a binary.
 *
 * The prompt is untrusted caller text, so it must always ride after a
 * `--` separator: without it, a prompt like `--model=x` or `-x` would
 * be parsed as a CLI option — rejecting the run, or rewriting model,
 * permission, or config choices. The assertion targets the argv helper
 * directly (deep import, outside the package export surface) so the
 * contract holds regardless of what binary is installed.
 */

import test from "node:test";
import assert from "node:assert/strict";
import { buildExecArgs } from "../dist/exec.js";

test("the prompt rides alone after the -- separator", () => {
  for (const prompt of ["-x", "--model=evil", "--", "hello world"]) {
    const args = buildExecArgs({ prompt });
    assert.equal(args[args.length - 2], "--", `separator before ${prompt}`);
    assert.equal(args[args.length - 1], prompt);
  }
});

test("surface options stay before the separator", () => {
  const args = buildExecArgs({
    prompt: "-x",
    approvals: true,
    model: "m",
    permissionMode: "auto",
    configPath: "cfg.toml",
  });
  const separator = args.indexOf("--");
  assert.equal(separator, args.length - 2);
  for (const flag of ["--approvals", "--model", "--permission-mode", "--config"]) {
    const at = args.indexOf(flag);
    assert.ok(at !== -1 && at < separator, `${flag} before the separator`);
  }
});
