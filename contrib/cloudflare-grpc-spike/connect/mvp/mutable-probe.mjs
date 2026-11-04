// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Exercise the Worker and SQLite-backed Durable Object in local workerd.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import {
  MutableLoadRequestSchema, MutableLoadResponseSchema,
  MutableStoreRequestSchema, MutableStoreResponseSchema,
  MutableCompareAndSwapRequestSchema, MutableCompareAndSwapResponseSchema,
} from "../src/gen/lore/storage/v1/storage_pb.js";

const origin = "http://127.0.0.1:8798";
const state = await mkdtemp(join(tmpdir(), "lore-mutable-"));
const repo = Buffer.alloc(16, 1).toString("base64");
const otherRepo = Buffer.alloc(16, 2).toString("base64");
const key = new Uint8Array(32).fill(10);
const zero = new Uint8Array(32);
const a = new Uint8Array(32).fill(1);
const b = new Uint8Array(32).fill(2);
let worker;

async function start() {
  worker = spawn("wrangler", ["dev", "--local", "--config", "mvp/wrangler.jsonc", "--persist-to", state, "--ip", "127.0.0.1", "--port", "8798"], { stdio: "inherit" });
  for (let i = 0; i < 150; i++) {
    if (worker.exitCode !== null) throw new Error(`Wrangler exited ${worker.exitCode}`);
    try { await fetch(origin); return; } catch { await delay(100); }
  }
  throw new Error("Wrangler startup timed out");
}

async function stop() {
  if (!worker || worker.exitCode !== null) return;
  const exited = new Promise(resolve => worker.once("exit", resolve));
  worker.kill("SIGTERM");
  await exited;
}

async function call(method, inputSchema, outputSchema, input, partition = repo) {
  const message = toBinary(inputSchema, create(inputSchema, input));
  const frame = Buffer.alloc(5 + message.length);
  frame.writeUInt32BE(message.length, 1);
  frame.set(message, 5);
  const response = await fetch(`${origin}/lore.storage.v1.StorageService/${method}`, {
    method: "POST",
    headers: { "content-type": "application/grpc-web+proto", "x-grpc-web": "1", ...(partition === null ? {} : { "lore-partition-bin": partition }) },
    body: frame,
  });
  assert.equal(response.status, 200);
  const data = Buffer.from(await response.arrayBuffer());
  let result;
  let trailer;
  for (let i = 0; i < data.length;) {
    assert.ok(i + 5 <= data.length);
    const length = data.readUInt32BE(i + 1);
    assert.ok(i + 5 + length <= data.length);
    const body = data.subarray(i + 5, i + 5 + length);
    if (data[i] === 0x80) trailer = body.toString();
    else result = fromBinary(outputSchema, Uint8Array.from(body));
    i += length + 5;
  }
  assert.ok(trailer, "missing gRPC-web trailer");
  const status = Number(/grpc-status:\s*(\d+)/i.exec(trailer)?.[1]);
  assert.ok(Number.isInteger(status), trailer);
  return { status, result };
}

const load = (type = 0, partition = repo, k = key) => call("MutableLoad", MutableLoadRequestSchema, MutableLoadResponseSchema, { key: k, keyType: type }, partition);
const store = (value, type = 0, partition = repo) => call("MutableStore", MutableStoreRequestSchema, MutableStoreResponseSchema, { key, keyType: type, value }, partition);
const cas = (expected, value, type = 0, partition = repo) => call("MutableCompareAndSwap", MutableCompareAndSwapRequestSchema, MutableCompareAndSwapResponseSchema, { key, keyType: type, expected, value }, partition);

try {
  await start();
  assert.equal((await load()).status, 5);
  assert.equal((await load(0, null)).status, 3);
  assert.equal((await load(0, "bad!")).status, 3);
  assert.equal((await load(8)).status, 3);
  assert.equal((await load(0, repo, new Uint8Array(3))).status, 3);
  for (const type of [1, 3, 4]) assert.equal((await store(a, type)).status, 3);
  assert.equal((await store(a)).status, 0);
  assert.deepEqual((await load()).result.value, a);
  assert.equal((await load(0, otherRepo)).status, 5);
  assert.equal((await load(2)).status, 5);
  const differentKey = key.slice();
  differentKey[2] ^= 255;
  assert.equal((await load(0, repo, differentKey)).status, 5);
  assert.deepEqual((await cas(b, b)).result.currentValue, a);
  assert.deepEqual((await load()).result.value, a);
  assert.deepEqual((await cas(a, b)).result.currentValue, a);
  assert.deepEqual((await load()).result.value, b);
  assert.equal((await store(zero)).status, 0);
  assert.equal((await load()).status, 5);
  assert.deepEqual((await cas(a, b)).result.currentValue, zero);
  assert.equal((await load()).status, 5);
  assert.deepEqual((await cas(zero, zero)).result.currentValue, zero);
  assert.deepEqual((await load()).result.value, zero);
  const contenders = await Promise.all(Array.from({ length: 24 }, (_, i) => cas(zero, new Uint8Array(32).fill(i + 1))));
  assert.equal(contenders.filter(response => Buffer.from(response.result.currentValue).equals(Buffer.from(zero))).length, 1);
  const winner = (await load()).result.value;
  assert.ok(winner.some(Boolean));
  await stop();
  await start();
  assert.deepEqual((await load()).result.value, winner);
  console.log("Mutable gRPC-web: validation, isolation, CAS contention, zero semantics and restart persistence passed");
} finally {
  await stop();
  await rm(state, { recursive: true, force: true });
}
