// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Exercises gRPC-web framing locally; this is not a native gRPC ingress test.
import { spawn } from "node:child_process";
import http from "node:http";
import { setTimeout as delay } from "node:timers/promises";

const origin = "http://127.0.0.1:8799";
const worker = spawn("wrangler", ["dev", "--local", "--ip", "127.0.0.1", "--port", "8799"], { stdio: "inherit" });
const frame = body => {
  const header = Buffer.alloc(5);
  header.writeUInt32BE(body.length, 1);
  return Buffer.concat([header, body]);
};

function call(path) {
  const request = http.request(origin + path, {
    method: "POST",
    headers: { "content-type": "application/grpc-web+proto", "x-grpc-web": "1" },
  });
  let buffer = Buffer.alloc(0);
  const frames = [];
  let ended = false;
  let failure;
  request.on("error", error => { failure = error; });
  request.on("response", response => {
    if (response.statusCode !== 200) failure = new Error(`HTTP ${response.statusCode}`);
    response.on("data", chunk => {
      buffer = Buffer.concat([buffer, chunk]);
      while (buffer.length >= 5 && buffer.length >= 5 + buffer.readUInt32BE(1)) {
        const length = buffer.readUInt32BE(1);
        frames.push(buffer.subarray(0, length + 5));
        buffer = buffer.subarray(length + 5);
      }
    });
    response.on("end", () => { ended = true; });
    response.on("error", error => { failure = error; });
  });
  return {
    request,
    async next() {
      for (let i = 0; i < 100; i++) {
        if (failure) throw failure;
        if (frames.length) return frames.shift();
        if (ended) throw new Error("stream ended before expected frame");
        await delay(100);
      }
      throw new Error("timed out waiting for frame");
    },
  };
}

function expectTrailer(value) {
  if (value[0] !== 0x80 || !/grpc-status:\s*0\r?\n/i.test(value.subarray(5).toString())) {
    throw new Error(`unexpected gRPC-web trailer: ${value.toString("hex")}`);
  }
}

try {
  for (let i = 0; i < 100; i++) {
    if (worker.exitCode !== null) throw new Error(`Wrangler exited ${worker.exitCode}`);
    try { await fetch(origin); break; } catch { if (i === 99) throw new Error("Wrangler startup timed out"); }
    await delay(100);
  }
  const env = call("/lore.environment.v1.EnvironmentService/EnvironmentGet");
  env.request.end(frame(Buffer.alloc(0)));
  if (!(await env.next()).equals(frame(Buffer.from([0x0a, 0x00])))) throw new Error("unexpected environment response");
  expectTrailer(await env.next());
  console.log("gRPC-web EnvironmentGet and terminal status succeeded");

  const get = call("/lore.storage.v1.StorageService/Get");
  for (const hash of [1, 2]) {
    const address = Buffer.from([0x0a, 1, hash]);
    get.request.write(frame(address));
    const reply = await get.next();
    const expected = Buffer.concat([Buffer.from([0x0a, address.length]), address]);
    if (reply[0] !== 0 || !reply.subarray(5).subarray(0, expected.length).equals(expected) ||
        !reply.subarray(5 + expected.length).includes(Buffer.from([0x08, 5]))) {
      throw new Error("unexpected Get response");
    }
    console.log(`gRPC-web Get item ${hash} received before request closed`);
  }
  get.request.end();
  expectTrailer(await get.next());
  console.log("gRPC-web Get terminal status succeeded");
} finally {
  worker.kill("SIGTERM");
}
