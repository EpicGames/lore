// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// The Wasm instance is used only by synchronous functions. No allocation or
// request-specific pointer is retained across an await or between calls.
import module from "../core/target/wasm32-unknown-unknown/release/lore_worker_core.wasm";

const { exports: core } = await WebAssembly.instantiate(module, {});

function withBytes(bytes, work) {
  const ptr = core.lore_alloc(bytes.length);
  try {
    new Uint8Array(core.memory.buffer, ptr, bytes.length).set(bytes);
    return work(ptr);
  } finally {
    core.lore_free(ptr, bytes.length);
  }
}

export function validFragmentMetadata(fragment) {
  return core.lore_fragment_metadata_valid(fragment.flags, fragment.sizePayload, fragment.sizeContent) === 1;
}

export function validFragmentList(fragment, payload) {
  return withBytes(payload, ptr => core.lore_fragment_list_valid(ptr, payload.length, fragment.sizeContent) === 1);
}

export function loreHash(payload) {
  return withBytes(payload, ptr => {
    const output = core.lore_alloc(32);
    try {
      core.lore_hash(ptr, payload.length, output);
      return new Uint8Array(core.memory.buffer, output, 32).slice();
    } finally {
      core.lore_free(output, 32);
    }
  });
}

export function validMetadata(bytes) {
  return withBytes(bytes, ptr => core.lore_metadata_valid(ptr, bytes.length) === 1);
}

export function encodeMetadata(entries) {
  const handle = core.lore_metadata_new();
  try {
    for (const [key, type, value] of entries) {
      const ok = withBytes(key, keyPtr => withBytes(value, valuePtr =>
        core.lore_metadata_set(handle, keyPtr, key.length, valuePtr, value.length, type)));
      if (!ok) throw new Error("invalid metadata entry");
    }
    return new Uint8Array(core.memory.buffer, core.lore_metadata_data(handle), core.lore_metadata_len(handle)).slice();
  } finally {
    core.lore_metadata_free(handle);
  }
}
