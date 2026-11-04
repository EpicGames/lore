// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Unauthenticated transport diagnostic. This is not a Lore server.
import { Code, ConnectError, createConnectRouter } from "@connectrpc/connect";
import { universalServerRequestFromFetch, universalServerResponseToFetch } from "@connectrpc/connect/protocol";
import { EnvironmentService } from "./gen/lore/environment/v1/environment_pb.js";
import { StorageService } from "./gen/lore/storage/v1/storage_pb.js";

const router = createConnectRouter();
router.service(EnvironmentService, {
  environmentGet: () => ({ environment: {} }),
});
router.service(StorageService, {
  async *get(addresses) {
    for await (const address of addresses) {
      if (address.hash.length === 1 && address.hash[0] === 0xfe) {
        throw new ConnectError("diagnostic stream failure", Code.ResourceExhausted);
      }
      yield { address, status: { code: 5, message: "transport spike: no data" } };
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
