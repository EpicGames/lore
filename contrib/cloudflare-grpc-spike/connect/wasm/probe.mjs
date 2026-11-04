// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Local gRPC-web stream probe: verify Rust results before closing the request.
import { spawn } from "node:child_process";
import http from "node:http";
import { setTimeout as delay } from "node:timers/promises";

const origin = "http://127.0.0.1:8800";
const worker = spawn("wrangler", ["dev", "--local", "--ip", "127.0.0.1", "--port", "8800"], { stdio: "inherit" });
const frame = body => {
  const header = Buffer.alloc(5);
  header.writeUInt32BE(body.length, 1);
  return Buffer.concat([header, body]);
};

try {
  for (let i = 0; i < 100; i++) {
    if (worker.exitCode !== null) throw new Error(`Wrangler exited ${worker.exitCode}`);
    try { await fetch(origin); break; } catch { if (i === 99) throw new Error("Wrangler startup timed out"); }
    await delay(100);
  }
  const request = http.request(`${origin}/lore.storage.v1.StorageService/Get`, {
    method: "POST", headers: { "content-type": "application/grpc-web+proto", "x-grpc-web": "1" },
  });
  let buffer = Buffer.alloc(0), ended = false, failure;
  const frames = [];
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
  async function next() {
    for (let i = 0; i < 100; i++) {
      if (failure) throw failure;
      if (frames.length) return frames.shift();
      if (ended) throw new Error("stream ended before expected frame");
      await delay(100);
    }
    throw new Error("timed out waiting for frame");
  }
  for (const [flags, expected] of [[0, 0], [2, 1], [4, 2], [8, 3], [16, 4], [3, 1]]) {
    const address = Buffer.from([0x0a, 1, flags]);
    request.write(frame(address));
    const reply = await next();
    const echoedAddress = Buffer.concat([Buffer.from([0x0a, address.length]), address]);
    if (reply[0] !== 0 || !reply.subarray(5).subarray(0, echoedAddress.length).equals(echoedAddress) ||
        !reply.subarray(5 + echoedAddress.length).includes(Buffer.from([0x08, 5])) ||
        !reply.subarray(5).includes(Buffer.from(`wasm-compression:${expected};domain:0`))) {
      throw new Error(`unexpected response for flags ${flags}: ${reply.toString("hex")}`);
    }
    console.log(`flags ${flags}: Rust result ${expected} before request closed`);
  }
  request.end();
  const trailer = await next();
  if (trailer[0] !== 0x80 || !/grpc-status:\s*0\r?\n/i.test(trailer.subarray(5).toString())) {
    throw new Error(`unexpected trailer: ${trailer.toString("hex")}`);
  }
  console.log("gRPC-web terminal status succeeded");
} finally {
  worker.kill("SIGTERM");
}
