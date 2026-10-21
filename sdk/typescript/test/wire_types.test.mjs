/**
 * The union in `src/types.ts` is a hand-maintained mirror of the Rust
 * `wavecode_wire::EventMsg`. These checks lock the mirror against the
 * fields the wire actually carries, so a Rust wire change that is not
 * mirrored here fails here instead of silently shipping a narrower type
 * to SDK consumers.
 */

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
// Line endings are normalised: the checkout may carry CRLF or LF, and
// the parsers below work on lines.
const types = readFileSync(path.join(here, "../src/types.ts"), "utf8").replace(
  /\r\n/g,
  "\n",
);

/**
 * The Rust wire crate's event enum: the authority on the wire tags.
 */
const wireSource = readFileSync(
  path.join(here, "../../../crates/foundation/wire/src/lib.rs"),
  "utf8",
).replace(/\r\n/g, "\n");

/**
 * The `EventMsg` variants the Rust enum declares, in source order.
 *
 * The enum body is the text between `pub enum EventMsg {` and the
 * closing brace at column zero. A variant is an identifier at the
 * enum's own indentation (four spaces); its fields sit deeper, so they
 * never match.
 */
function rustEventVariants() {
  const start = wireSource.indexOf("pub enum EventMsg {");
  assert.notEqual(start, -1, "the wire crate declares EventMsg");
  const body = wireSource.slice(start);
  const end = body.indexOf("\n}");
  assert.notEqual(end, -1, "the EventMsg enum is closed");
  const variants = [
    ...body.slice(0, end).matchAll(/^ {4}([A-Z][A-Za-z0-9]*)[ \t]*(\{|,)/gm),
  ].map((match) => match[1]);
  assert.ok(variants.length > 0, "the EventMsg enum declares variants");
  return variants;
}

/**
 * The wire tag serde derives from a variant name.
 *
 * `#[serde(rename_all = "snake_case")]` turns a variant name into its
 * tag by putting an underscore before every capital after the first
 * and lowercasing the rest.
 */
function wireTag(variant) {
  let tag = "";
  for (const [index, ch] of [...variant].entries()) {
    if (index > 0 && ch === ch.toUpperCase() && ch !== ch.toLowerCase()) {
      tag += "_";
    }
    tag += ch.toLowerCase();
  }
  return tag;
}

/**
 * The tags the TypeScript union declares, in source order.
 *
 * The union is the text between `export type WireEventPayload =` and
 * the blank line that ends it, so a `type` field declared by another
 * type in the file is not mistaken for a wire tag.
 */
function typescriptTags() {
  const start = types.indexOf("export type WireEventPayload =");
  assert.notEqual(start, -1, "the SDK declares the wire payload union");
  const end = types.indexOf("\n\n", start);
  assert.notEqual(end, -1, "the wire payload union is closed");
  return [...types.slice(start, end).matchAll(/type: "([a-z0-9_]+)"/g)].map(
    (match) => match[1],
  );
}

/**
 * The union member that declares `tag`, brace-matched so a member with a
 * nested object type (`tool_call_end`'s preview) is returned whole.
 */
function member(tag) {
  const marker = `type: "${tag}"`;
  const at = types.indexOf(marker);
  assert.notEqual(at, -1, `no union member declares the "${tag}" payload`);
  const open = types.lastIndexOf("{", at);
  let depth = 0;
  for (let i = open; i < types.length; i += 1) {
    if (types[i] === "{") {
      depth += 1;
    } else if (types[i] === "}") {
      depth -= 1;
      if (depth === 0) {
        return types.slice(open, i + 1);
      }
    }
  }
  throw new Error(`unbalanced braces around the "${tag}" member`);
}

test("turn_started mirrors the wire model field", () => {
  assert.match(member("turn_started"), /model\?: string/);
});

test("tool_call_end mirrors the wire outcome and duration fields", () => {
  const end = member("tool_call_end");
  assert.match(end, /outcome\?: ToolOutcome/);
  assert.match(end, /duration_ms\?: number/);
});

test("the ToolOutcome union lists every dispatch exit", () => {
  const outcomes = [
    "executed",
    "denied",
    "refused",
    "hook_blocked",
    "surface_blocked",
    "interrupted",
    "duplicate",
    "answered",
    "ask_unavailable",
    "missing",
  ];
  for (const tag of outcomes) {
    assert.ok(
      types.includes(`| "${tag}"`),
      `ToolOutcome misses "${tag}"`,
    );
  }
});

test("the approval decision dialect accepts a bare deny", () => {
  assert.match(types, /export type ApprovalDecision =[^;]*"deny"/);
});

test("every Rust wire event has a TypeScript union member", () => {
  const variants = rustEventVariants();
  const tags = new Set(typescriptTags());
  for (const variant of variants) {
    const tag = wireTag(variant);
    assert.ok(
      tags.has(tag),
      `the SDK union misses the "${tag}" payload of EventMsg::${variant}`,
    );
  }
});

test("the TypeScript union carries no payload the Rust enum dropped", () => {
  const known = new Set(rustEventVariants().map(wireTag));
  for (const tag of typescriptTags()) {
    assert.ok(
      known.has(tag),
      `the SDK union declares "${tag}", which EventMsg no longer has`,
    );
  }
});
