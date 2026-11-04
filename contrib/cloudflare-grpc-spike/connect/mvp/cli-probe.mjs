// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Local mode bridges gRPC to gRPC-web. Remote mode forwards native gRPC over
// TLS using Node, allowing tests where Lore's TLS rejects an intercepting CA.
import { spawn } from "node:child_process";
import http from "node:http";
import http2 from "node:http2";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { randomUUID } from "node:crypto";

const target = process.argv[2] ? new URL(process.argv[2]) : null;
if (target && (target.protocol !== "https:" || target.pathname !== "/" || target.search || target.hash)) throw new Error("usage: node mvp/cli-probe.mjs [https://HOST]");
const temp = await mkdtemp(join(tmpdir(), "lore-cli-worker-"));
const work = join(temp, "work");
await mkdir(work);
const worker = target ? null : spawn("wrangler", ["dev", "--local", "--config", "mvp/wrangler.jsonc", "--persist-to", join(temp, "state"), "--ip", "127.0.0.1", "--port", "8796"], { stdio: "inherit" });
const bridge = http2.createServer();
const remote = target ? http2.connect(target.origin) : null;
if (remote) remote.on("error", error => console.error(`upstream: ${error.message}`));
if (remote) bridge.on("stream", (client, headers) => {
  const outgoing = remote.request({ ...headers, ":scheme": "https", ":authority": target.host });
  let trailers;
  let ready = false;
  client.on("wantTrailers", () => { ready = true; client.sendTrailers(trailers ?? {}); });
  outgoing.on("response", response => client.respond(response, { waitForTrailers: true }));
  outgoing.on("trailers", value => { trailers = value; });
  outgoing.on("error", error => { console.error(`upstream stream: ${error.message}`); client.close(); });
  client.on("error", () => outgoing.close());
  client.on("close", () => { if (!ready) outgoing.close(); });
  client.pipe(outgoing);
  outgoing.pipe(client);
});
else bridge.on("stream", (client, headers) => {
  const incoming = Object.fromEntries(Object.entries(headers).filter(([key]) => !key.startsWith(":")));
  incoming["content-type"] = "application/grpc-web+proto";
  incoming["x-grpc-web"] = "1";
  delete incoming.te;
  const upstream = http.request({ hostname: "127.0.0.1", port: 8796, path: headers[":path"], method: "POST", headers: incoming });
  let trailers = { "grpc-status": "13", "grpc-message": "missing grpc-web trailer" };
  client.on("wantTrailers", () => client.sendTrailers(trailers));
  upstream.on("response", response => {
    client.respond({ ":status": 200, "content-type": "application/grpc" }, { waitForTrailers: true });
    let data = Buffer.alloc(0);
    response.on("data", chunk => {
      data = Buffer.concat([data, chunk]);
      while (data.length >= 5 && data.length >= 5 + data.readUInt32BE(1)) {
        const size = data.readUInt32BE(1);
        const message = data.subarray(0, size + 5);
        data = data.subarray(size + 5);
        if (message[0] & 0x80) {
          trailers = Object.fromEntries(message.subarray(5).toString().split("\r\n").filter(Boolean).map(line => {
            const colon = line.indexOf(":");
            return [line.slice(0, colon).toLowerCase(), line.slice(colon + 1).trim()];
          }));
        } else client.write(message);
      }
    });
    response.on("end", () => client.end());
    response.on("error", () => client.close());
  });
  upstream.on("error", error => { console.error(error); client.close(); });
  client.on("error", () => upstream.destroy());
  client.pipe(upstream);
});
const binary = resolve("../../../target/debug/lore");
const token = [Buffer.from('{"alg":"none"}').toString("base64url"), Buffer.from('{"sub":"tester","iss":"local","aud":"local","exp":4102444800}').toString("base64url"), "x"].join(".");
async function lore(args, cwd = work) {
  console.log(`\n$ lore ${args.join(" ")}`);
  const child = spawn(binary, ["--identity", "tester", ...args], { cwd, stdio: "inherit", env: { ...process.env, LORE_ACCESS_TOKEN: token } });
  const code = await new Promise((resolve, reject) => { child.on("error", reject); child.on("exit", resolve); });
  if (code !== 0) throw new Error(`lore exited with status ${code}`);
}
try {
  for (let i = 0; worker && i < 150; i++) {
    if (worker.exitCode !== null) throw new Error(`Wrangler exited ${worker.exitCode}`);
    try { await fetch("http://127.0.0.1:8796"); break; } catch { await delay(100); }
  }
  await new Promise(resolve => bridge.listen(0, "127.0.0.1", resolve));
  const url = `grpc://127.0.0.1:${bridge.address().port}`;
  const name = target ? `probe-${randomUUID()}` : "demo";
  await lore(["repository", "create", `${url}/${name}`]);
  await lore(["repository", "list", url]);
  await writeFile(join(work, "example.txt"), "Hello from Lore on Workers.\n");
  await lore(["stage", "example.txt"]);
  await lore(["commit", "initial commit"]);
  await lore(["push"]);
  await writeFile(join(work, "example.txt"), "A second revision on Workers.\n");
  await lore(["stage", "example.txt"]);
  await lore(["commit", "second commit"]);
  await lore(["push"]);
  await lore(["clone", `${url}/${name}`, join(temp, "clone")], temp);
  assert.equal(await readFile(join(temp, "clone", "example.txt"), "utf8"), "A second revision on Workers.\n");
  console.log("CLI create, list, stage, commit, push and clone passed");
} finally {
  bridge.close();
  if (remote) remote.destroy();
  if (worker && worker.exitCode === null) {
    const exited = new Promise(resolve => worker.once("exit", resolve));
    worker.kill("SIGTERM");
    await exited;
  }
  await rm(temp, { recursive: true, force: true });
}
