// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Run the same metadata wire fixture as lore-revision's native unit test.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const wasm = await readFile(new URL("./core/target/wasm32-unknown-unknown/release/lore_worker_core.wasm", import.meta.url));
const { instance: { exports: core } } = await WebAssembly.instantiate(wasm);
const bytes = text => new TextEncoder().encode(text);
function withBytes(value, fn) {
  const ptr = core.lore_alloc(value.length);
  try {
    new Uint8Array(core.memory.buffer, ptr, value.length).set(value);
    return fn(ptr);
  } finally { core.lore_free(ptr, value.length); }
}
function valid(buffer) { return withBytes(buffer, ptr => core.lore_metadata_valid(ptr, buffer.length) === 1); }
const handle = core.lore_metadata_new();
try {
  function set(key, value, tag) {
    const k = bytes(key);
    assert.equal(withBytes(k, kp => withBytes(value, vp => core.lore_metadata_set(handle, kp, k.length, vp, value.length, tag))), 1);
  }
  function output() { return new Uint8Array(core.memory.buffer, core.lore_metadata_data(handle), core.lore_metadata_len(handle)).slice(); }
  set("a", bytes("one"), 6);
  set("b", Uint8Array.of(1), 2);
  set("a", bytes("two"), 255);
  const expected = Uint8Array.of(
    0x61, 0x74, 0x65, 0x6d, 1, 0, 0, 0,
    1, 0, 0, 0, 3, 0, 0, 0, 255, 0, 0, 0, 97, 116, 119, 111,
    1, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 98, 1,
  );
  assert.deepEqual(output(), expected);
  assert(valid(expected));
  for (let length = 1; length < expected.length; length++) {
    // A header alone and a complete prefix of entries are also valid buffers.
    assert.equal(valid(expected.slice(0, length)), length === 8 || length === 24);
  }
  const forged = expected.slice();
  forged.fill(255, 8, 16);
  assert(!valid(forged));
  set("a", bytes("longer"), 6);
  const replaced = output();
  assert(valid(replaced));
  assert.equal(String.fromCharCode(replaced[20]), "b");
} finally { core.lore_metadata_free(handle); }

const list = new Uint8Array(80);
const offsets = new DataView(list.buffer);
const validList = size => withBytes(list, ptr => core.lore_fragment_list_valid(ptr, list.length, size) === 1);
offsets.setBigUint64(32, 10n, true);
offsets.setBigUint64(72, 20n, true);
assert(validList(11n));
assert(!validList(10n)); // second fragment starts at the end of the window
offsets.setBigUint64(72, 10n, true);
assert(!validList(11n)); // offsets must increase
offsets.setBigUint64(32, (1n << 64n) - 2n, true);
offsets.setBigUint64(72, (1n << 64n) - 1n, true);
assert(!validList(2n)); // content window overflows u64
assert.equal(core.lore_fragment_metadata_valid(0, 10, 10n), 1);
assert.equal(core.lore_fragment_metadata_valid(0, 10, 11n), 0);
assert.equal(core.lore_fragment_metadata_valid(1, 80, 80n), 1);
console.log("Native/Wasm metadata fixture, malformed buffers and fragment bounds passed");
