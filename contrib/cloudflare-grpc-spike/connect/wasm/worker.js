// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Local-only diagnostic: the first byte of the hash is a synthetic flags value.
import module from "./target/wasm32-unknown-unknown/release/lore_fragment_flags_spike.wasm";
import { createConnectRouter } from "@connectrpc/connect";
import { universalServerRequestFromFetch, universalServerResponseToFetch } from "@connectrpc/connect/protocol";
import { StorageService } from "../src/gen/lore/storage/v1/storage_pb.js";

const instance = await WebAssembly.instantiate(module, {});
const router = createConnectRouter();
router.service(StorageService, {
  async *get(addresses) {
    for await (const address of addresses) {
      const code = instance.exports.compression_code(address.hash[0] ?? 0);
      const domain = instance.exports.domain_check();
      yield { address, status: { code: 5, message: `wasm-compression:${code};domain:${domain}` } };
    }
  },
});
const handlers = new Map(router.handlers.map(handler => [handler.requestPath, handler]));

export default {
  async fetch(request) {
    const handler = handlers.get(new URL(request.url).pathname);
    if (!handler) return new Response("Not found", { status: 404 });
    return universalServerResponseToFetch(await handler(universalServerRequestFromFetch(request, {})));
  },
};
