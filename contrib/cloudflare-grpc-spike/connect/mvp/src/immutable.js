// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
import { Code, ConnectError } from "@connectrpc/connect";
import { Decompress } from "fzstd";
import { loreHash, validFragmentList, validFragmentMetadata } from "./core.js";

const MAX_FRAGMENT = 256 * 1024;
const STORED = 0x1c0000;
const DURABLE = 0x040000;
const UINT64_MAX = (1n << 64n) - 1n;
export const toHex = data => Array.from(data, byte => byte.toString(16).padStart(2, "0")).join("");
export const fromHex = value => Uint8Array.from(value.match(/../g), part => parseInt(part, 16));

function invalid(message) { return new ConnectError(message, Code.InvalidArgument); }

function contextHex(context) {
  if (context.length === 0) return "00".repeat(16);
  if (context.length !== 16) throw invalid("context must be 16 bytes");
  return toHex(context);
}

function addressParts(address) {
  if (!address || address.hash.length !== 32) throw invalid("address hash must be 32 bytes");
  return { hash: toHex(address.hash), context: contextHex(address.context) };
}

// Bound the Zstd frame window before passing untrusted input to the decoder.
// Accept one ordinary frame without dictionaries or checksums; reject other
// representations instead of silently accepting a partially verified payload.
function decodeZstd(payload, size) {
  if (payload.length < 6 || payload[0] !== 0x28 || payload[1] !== 0xb5 || payload[2] !== 0x2f || payload[3] !== 0xfd) throw invalid("invalid Zstd frame");
  const descriptor = payload[4];
  if (descriptor & 0x1f) throw invalid("unsupported Zstd frame features");
  const single = (descriptor & 0x20) !== 0;
  const mode = descriptor >> 6;
  let pos = 5;
  let window = 0;
  if (!single) {
    const wd = payload[pos++];
    const base = 2 ** (10 + (wd >> 3));
    window = base + base / 8 * (wd & 7);
    if (window > MAX_FRAGMENT) throw invalid("Zstd window exceeds fragment limit");
  }
  const fieldSize = mode === 0 ? (single ? 1 : 0) : 2 ** mode;
  if (pos + fieldSize > payload.length) throw invalid("truncated Zstd header");
  let contentSize = 0n;
  for (let i = 0; i < fieldSize; i++) contentSize |= BigInt(payload[pos++]) << BigInt(i * 8);
  if (mode === 1) contentSize += 256n;
  if (fieldSize && contentSize !== BigInt(size)) throw invalid("Zstd content size mismatch");
  if (single && contentSize > BigInt(MAX_FRAGMENT)) throw invalid("Zstd window exceeds fragment limit");
  let last = false;
  while (!last) {
    if (pos + 3 > payload.length) throw invalid("truncated Zstd block");
    const header = payload[pos] | payload[pos + 1] << 8 | payload[pos + 2] << 16;
    pos += 3;
    last = (header & 1) !== 0;
    const type = (header >> 1) & 3;
    const length = header >> 3;
    if (type === 3 || length > 131072) throw invalid("invalid Zstd block");
    pos += type === 1 ? 1 : length;
    if (pos > payload.length) throw invalid("truncated Zstd block");
  }
  if (pos !== payload.length) throw invalid("extra data after Zstd frame");
  const chunks = [];
  let total = 0;
  try {
    const decoder = new Decompress(chunk => {
      total += chunk.length;
      if (total > size) throw invalid("Zstd output exceeds declared size");
      chunks.push(chunk);
    });
    decoder.push(payload, true);
  } catch (error) {
    if (error instanceof ConnectError) throw error;
    throw invalid("invalid Zstd payload");
  }
  if (total !== size) throw invalid("Zstd output size mismatch");
  const result = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) { result.set(chunk, offset); offset += chunk.length; }
  return result;
}

function validateFragment(fragment, payload, expectedHash) {
  if (!fragment || !payload) throw invalid("fragment and payload are required");
  const { flags, sizePayload, sizeContent } = fragment;
  const compressed = flags & 0xfe;
  const fragmented = (flags & 1) !== 0;
  if (!validFragmentMetadata(fragment) || payload.length !== sizePayload) {
    throw invalid("invalid fragment metadata or payload size");
  }
  if (compressed && compressed !== 8) throw new ConnectError("only Zstd compressed fragments are supported", Code.Unimplemented);
  const content = compressed ? decodeZstd(payload, Number(sizeContent)) : payload;
  if (fragmented && !validFragmentList(fragment, payload)) throw invalid("invalid fragment reference list");
  if (toHex(loreHash(content)) !== expectedHash) throw invalid("fragment content hash mismatch");
  return content;
}

function objectKey(partition, hash) { return `fragments/${partition}/${hash}`; }

function fragmentFromObject(object) {
  const flags = Number(object.customMetadata.flags);
  const sizePayload = Number(object.customMetadata.payload);
  const content = object.customMetadata.content;
  if (!Number.isInteger(flags) || !Number.isInteger(sizePayload) || sizePayload <= 0 ||
      sizePayload > MAX_FRAGMENT || sizePayload !== object.size || !/^\d+$/.test(content ?? "")) {
    throw new ConnectError("invalid stored fragment metadata", Code.DataLoss);
  }
  const sizeContent = BigInt(content);
  if (sizeContent > UINT64_MAX) throw new ConnectError("invalid stored content size", Code.DataLoss);
  return { flags: (flags & ~STORED) | DURABLE, sizePayload, sizeContent };
}

export async function putFragment(env, scope, address, fragment, payload) {
  const parts = addressParts(address);
  const content = validateFragment(fragment, payload, parts.hash);
  // Normalize compressed input to a verified plain representation. Clients can
  // read it regardless of the encoding used by the writer.
  const storedFlags = fragment.flags & ~STORED & ~0xfe;
  const key = objectKey(scope.id, parts.hash);
  // R2 stores the complete representation atomically before its association is
  // published. A crash can leave an unreferenced object, never a dangling row.
  const created = await env.FRAGMENTS.put(key, content, {
    onlyIf: new Headers({ "if-none-match": "*" }),
    customMetadata: {
      flags: String(storedFlags),
      payload: String(content.length),
      content: String(fragment.sizeContent),
    },
  });
  if (!created) {
    const existing = await env.FRAGMENTS.head(key);
    if (!existing) throw new ConnectError("fragment disappeared during put", Code.Unavailable);
    const stored = fragmentFromObject(existing);
    // A plain payload and a fragment list can have identical bytes but different
    // interpretations. Never claim that the conflicting representation was stored.
    if ((stored.flags & 1) !== (fragment.flags & 1) || stored.sizeContent !== fragment.sizeContent) {
      throw new ConnectError("fragment conflicts with existing representation", Code.AlreadyExists);
    }
  }
  await scope.store.associate(parts.hash, parts.context);
}

export async function getFragment(env, scope, address, metadataOnly = false) {
  const parts = addressParts(address);
  if (await scope.store.matchFragment(parts.hash, parts.context) === 2) {
    throw new ConnectError("fragment not found", Code.NotFound);
  }
  const key = objectKey(scope.id, parts.hash);
  const object = metadataOnly ? await env.FRAGMENTS.head(key) : await env.FRAGMENTS.get(key);
  if (!object) throw new ConnectError("fragment association has no payload", Code.DataLoss);
  const fragment = fragmentFromObject(object);
  const payload = metadataOnly ? new Uint8Array() : new Uint8Array(await object.arrayBuffer());
  return { fragment, payload };
}

export async function queryFragments(scope, addresses) {
  if (addresses.length > Math.floor(MAX_FRAGMENT / 48)) throw invalid("too many addresses");
  const results = [];
  for (const address of addresses) {
    const parts = addressParts(address);
    results.push(await scope.store.matchFragment(parts.hash, parts.context));
  }
  return { results };
}

export function itemStatus(error) {
  if (error instanceof ConnectError) return { code: error.code, message: error.rawMessage };
  console.error(JSON.stringify({ message: "fragment operation failed", error: String(error) }));
  return { code: Code.Internal, message: "fragment operation failed" };
}
