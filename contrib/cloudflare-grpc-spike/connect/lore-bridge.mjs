// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Loopback-only diagnostic: forwards HTTP/2 while rewriting the upstream authority.
// TLS is verified by Node rather than Lore; this is not a direct TLS test.
import http2 from "node:http2";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const target = new URL(process.argv[2]);
if (target.protocol !== "https:") throw new Error("usage: node lore-bridge.mjs https://HOST");
const upstream = http2.connect(target.origin);
const server = http2.createServer();
upstream.on("error", error => console.error(`upstream: ${error.message}`));
server.on("stream", (incoming, headers) => {
  const outgoing = upstream.request({ ...headers, ":scheme": "https", ":authority": target.host });
  let trailers;
  let readyForTrailers = false;
  incoming.on("wantTrailers", () => {
    readyForTrailers = true;
    incoming.sendTrailers(trailers ?? {});
  });
  outgoing.on("response", response => incoming.respond(response, { waitForTrailers: true }));
  outgoing.on("trailers", value => { trailers = value; });
  outgoing.on("error", error => { console.error(`upstream stream: ${error.message}`); incoming.close(); });
  incoming.on("error", error => { console.error(`client stream: ${error.message}`); outgoing.close(); });
  incoming.on("close", () => { if (!readyForTrailers) outgoing.close(); });
  incoming.pipe(outgoing);
  outgoing.pipe(incoming);
});
server.listen(0, "127.0.0.1", () => {
  const port = server.address().port;
  const binary = fileURLToPath(new URL("../../../target/debug/lore", import.meta.url));
  const client = spawn(binary, ["--debug", "repository", "list", `grpc://127.0.0.1:${port}`], { stdio: "inherit" });
  const finish = code => { server.close(); upstream.destroy(); process.exitCode = code; };
  client.on("exit", code => finish(code ?? 1));
  client.on("error", error => { console.error(error); finish(1); });
});
