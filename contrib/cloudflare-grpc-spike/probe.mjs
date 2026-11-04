// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
import { spawn } from "node:child_process";
import http from "node:http";
import http2 from "node:http2";
import { setTimeout as delay } from "node:timers/promises";

const target = process.argv[2];
const local = process.argv.includes("--local");
const diagnostic = process.argv.includes("--octet-stream");
if (!target || (local && !target.startsWith("http://")) || (!local && !target.startsWith("https://"))) {
  console.error("Usage: node probe.mjs http://127.0.0.1:8788 --local | https://worker.example.com");
  process.exit(2);
}

const environmentPath = "/lore.environment.v1.EnvironmentService/EnvironmentGet";
const storagePath = "/lore.storage.v1.StorageService/Get";
let child;
let session;

function frame(body) {
  const header = Buffer.alloc(5);
  header.writeUInt32BE(body.length, 1);
  return Buffer.concat([header, body]);
}

function request(path) {
  const headers = { "content-type": diagnostic ? "application/octet-stream" : "application/grpc", te: "trailers" };
  if (local) return http.request(new URL(path, target), { method: "POST", headers });
  return session.request({ ":method": "POST", ":path": path, ...headers });
}

function receive(req, isHttp1 = local) {
  let headers;
  let trailers = {};
  let response;
  let buffer = Buffer.alloc(0);
  let ended = false;
  let failure;
  const frames = [];
  let wake = () => {};
  const notify = () => wake();
  function data(chunk) {
    buffer = Buffer.concat([buffer, chunk]);
    while (buffer.length >= 5 && buffer.length >= 5 + buffer.readUInt32BE(1)) {
      const size = buffer.readUInt32BE(1);
      frames.push(buffer.subarray(0, size + 5));
      buffer = buffer.subarray(size + 5);
    }
    notify();
  }
  function finished() { ended = true; notify(); }
  function failed(error) { failure = error; notify(); }
  if (isHttp1) {
    req.on("response", res => {
      response = res;
      headers = res.headers;
      res.on("data", data);
      res.on("end", () => { trailers = res.trailers; finished(); });
      res.on("error", failed);
      notify();
    });
  } else {
    req.on("response", value => { headers = value; notify(); });
    req.on("data", data);
    req.on("trailers", value => { trailers = value; });
    req.on("end", finished);
  }
  req.on("error", failed);
  async function waitFor(predicate, label) {
    const deadline = Date.now() + 5000;
    while (!predicate()) {
      if (failure) throw failure;
      if (ended) throw new Error(`${label}: stream ended early`);
      const remaining = deadline - Date.now();
      if (remaining <= 0) throw new Error(`${label}: timed out`);
      await new Promise(resolve => {
        const timer = setTimeout(resolve, remaining);
        wake = () => { clearTimeout(timer); resolve(); };
      });
    }
  }
  return {
    async headers() { await waitFor(() => headers, "response headers"); return headers; },
    async frame() { await waitFor(() => frames.length > 0, "response frame"); return frames.shift(); },
    async end() { await waitFor(() => ended, "response end"); return trailers; },
    close() { response?.destroy(); req.destroy(); },
  };
}

async function run() {
  if (diagnostic) console.log("Diagnostic only: application/octet-stream is not a Lore gRPC request");
  if (local) {
    child = spawn("wrangler", ["dev", "--ip", "127.0.0.1", "--port", new URL(target).port, "--local"], { stdio: "inherit" });
    // Retry the connection while the custom build and local runtime start.
    for (let i = 0; i < 90; i++) {
      if (child.exitCode !== null) throw new Error(`wrangler exited: ${child.exitCode}`);
      try {
        await fetch(target);
        break;
      } catch (error) {
        if (i === 89) throw error;
        await delay(1000);
      }
    }
  } else {
    session = http2.connect(target);
    session.on("error", error => console.error(`HTTP/2 session: ${error.message}`));
  }

  const envReq = request(environmentPath);
  const env = receive(envReq);
  envReq.end(frame(Buffer.alloc(0)));
  const envHeaders = await env.headers();
  const status = envHeaders[local ? "status" : ":status"];
  if (!local && status !== 200) {
    throw new Error(`environment HTTP status ${status}; content-type=${envHeaders["content-type"] ?? "absent"}`);
  }
  const envFrame = await env.frame();
  const envTrailers = await env.end();
  if (!envFrame.equals(frame(Buffer.from([0x0a, 0x00])))) throw new Error("unexpected environment message");
  console.log(`environment response received; initial grpc-status=${envHeaders["grpc-status"] ?? "absent"}; trailer grpc-status=${envTrailers["grpc-status"] ?? "absent"}`);

  const getReq = request(storagePath);
  const get = receive(getReq);
  // Address { hash: [1] }, followed only after its answer by hash [2].
  const addresses = [Buffer.from([0x0a, 0x01, 1]), Buffer.from([0x0a, 0x01, 2])];
  for (const address of addresses) {
    getReq.write(frame(address));
    const reply = await get.frame();
    const expectedPrefix = Buffer.concat([Buffer.from([0x0a, address.length]), address]);
    if (!reply.subarray(5).subarray(0, expectedPrefix.length).equals(expectedPrefix)) throw new Error("response address mismatch");
    if (!reply.subarray(5 + expectedPrefix.length).includes(Buffer.from([0x08, 0x05]))) throw new Error("response lacks NOT_FOUND item status");
    console.log(`received response for hash ${address.at(-1)} before closing request`);
  }
  getReq.end();
  const trailers = await get.end();
  if (trailers["grpc-status"] !== "0") throw new Error(`missing successful terminal gRPC trailer: ${trailers["grpc-status"] ?? "absent"}`);
  console.log("terminal gRPC status received");
}

try {
  await run();
} catch (error) {
  console.error(`FAIL: ${error.message}`);
  process.exitCode = 1;
} finally {
  session?.destroy();
  child?.kill("SIGTERM");
}
