// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Native gRPC ingress diagnostic: status conversion, cancellation and reconnect.
import http2 from "node:http2";
import { once } from "node:events";

const target = process.argv[2];
if (!target?.startsWith("https://")) throw new Error("usage: node edge-errors.mjs https://HOST");
const frame = body => {
  const header = Buffer.alloc(5);
  header.writeUInt32BE(body.length, 1);
  return Buffer.concat([header, body]);
};
const address = hash => frame(Buffer.from([0x0a, 1, hash]));
const headers = path => ({ ":method": "POST", ":path": path, "content-type": "application/grpc", te: "trailers" });
const getPath = "/lore.storage.v1.StorageService/Get";
const envPath = "/lore.environment.v1.EnvironmentService/EnvironmentGet";
const timeout = setTimeout(() => { console.error("probe timed out"); process.exit(1); }, 15000);
let session;
try {
  session = http2.connect(target);
  const errorCall = session.request(headers(getPath));
  let errorTrailers;
  errorCall.on("trailers", value => { errorTrailers = value; });
  errorCall.resume();
  errorCall.end(address(0xfe));
  await once(errorCall, "end");
  if (errorTrailers?.["grpc-status"] !== "8") throw new Error(`expected RESOURCE_EXHAUSTED, got ${JSON.stringify(errorTrailers)}`);
  if (!errorTrailers["grpc-message"]?.includes("diagnostic%20stream%20failure")) throw new Error("missing diagnostic grpc-message");
  console.log("stream failure converted to RESOURCE_EXHAUSTED trailers");

  const cancelled = session.request(headers(getPath));
  cancelled.write(address(1));
  await once(cancelled, "data");
  cancelled.close(http2.constants.NGHTTP2_CANCEL);
  await once(cancelled, "close");
  console.log("open Get stream cancelled after receiving a response");

  session.destroy();
  session = http2.connect(target);
  const discovery = session.request(headers(envPath));
  let trailers;
  let response = Buffer.alloc(0);
  discovery.on("trailers", value => { trailers = value; });
  discovery.on("data", chunk => { response = Buffer.concat([response, chunk]); });
  discovery.end(frame(Buffer.alloc(0)));
  await once(discovery, "end");
  if (!response.equals(frame(Buffer.from([0x0a, 0x00]))) || trailers?.["grpc-status"] !== "0") {
    throw new Error("discovery after reconnect failed");
  }
  console.log("discovery succeeded on a fresh connection");
} finally {
  clearTimeout(timeout);
  session?.destroy();
}
