/**
 * Auto-discovery of Lore servers on the local network. Attempts to find Lore
 * backend servers without user configuration, falling back to manual entry.
 */

import { createConnection } from "node:net";
import { log } from "./log.mjs";

/**
 * Test if a Lore server is reachable at the given address.
 * @param {string} host hostname or IP address
 * @param {number} port port number
 * @param {number} timeout milliseconds to wait before considering unreachable
 * @returns {Promise<boolean>} true if server responds, false otherwise
 */
function testServer(host, port, timeout = 1000) {
  return new Promise((resolve) => {
    const socket = createConnection({ host, port, timeout });
    const timer = setTimeout(() => {
      socket.destroy();
      resolve(false);
    }, timeout);

    socket.on("connect", () => {
      clearTimeout(timer);
      socket.destroy();
      resolve(true);
    });

    socket.on("error", () => {
      clearTimeout(timer);
      resolve(false);
    });
  });
}

/**
 * Discover Lore servers on the local network. Tries common addresses and
 * returns a list of reachable servers with their full URLs.
 * @param {Object} options
 * @param {number} options.timeout milliseconds to wait per host (default: 500)
 * @returns {Promise<Array<{url: string, label: string}>>} discovered server URLs
 */
export async function discoverServers(options = {}) {
  const { timeout = 500 } = options;
  const discovered = [];

  const candidates = [
    { host: "localhost", port: 41337, label: "localhost" },
    { host: "127.0.0.1", port: 41337, label: "127.0.0.1" },
    { host: "lore.local", port: 41337, label: "lore.local" },
  ];

  const results = await Promise.all(
    candidates.map(async (c) => {
      const reachable = await testServer(c.host, c.port, timeout);
      if (reachable) {
        return { url: `lore://${c.host}:${c.port}`, label: c.label };
      }
      return null;
    }),
  );

  for (const result of results) {
    if (result) discovered.push(result);
  }

  if (discovered.length > 0) {
    log.debug("discovered Lore servers", { count: discovered.length, servers: discovered });
  }

  return discovered;
}
