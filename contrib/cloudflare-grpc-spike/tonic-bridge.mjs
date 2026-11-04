// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Diagnostic only: a loopback TCP-to-TLS bridge, never a production proxy.
// It lets tonic exercise the public HTTP/2 endpoint when local TLS interception
// presents a certificate unsupported by rustls. TLS remains verified by Node.
import { spawn } from "node:child_process";
import net from "node:net";
import tls from "node:tls";

const target = new URL(process.argv[2] ?? "https://lore.gasparina.work");
if (target.protocol !== "https:") throw new Error("HTTPS target required");
const sockets = new Set();
const server = net.createServer(client => {
  const upstream = tls.connect({ host: target.hostname, port: Number(target.port || 443), servername: target.hostname, ALPNProtocols: ["h2"] });
  sockets.add(client);
  sockets.add(upstream);
  client.on("close", () => { sockets.delete(client); upstream.destroy(); });
  upstream.on("close", () => { sockets.delete(upstream); client.destroy(); });
  client.on("error", error => console.error(`bridge client: ${error.message}`));
  upstream.on("error", error => { console.error(`bridge TLS: ${error.message}`); client.destroy(); });
  upstream.on("secureConnect", () => {
    if (upstream.alpnProtocol !== "h2") return upstream.destroy(new Error("upstream did not negotiate h2"));
    client.pipe(upstream).pipe(client);
  });
});
server.listen(0, "127.0.0.1", () => {
  const port = server.address().port;
  const child = spawn("cargo", ["run", "--locked", "--example", "tonic_probe", "--", `http://127.0.0.1:${port}`], {
    stdio: "inherit", env: { ...process.env, LORE_SPIKE_ORIGIN: target.origin },
  });
  child.on("exit", code => {
    for (const socket of sockets) socket.destroy();
    server.close();
    process.exitCode = code ?? 1;
  });
  child.on("error", error => { console.error(error); server.close(); process.exitCode = 1; });
});
