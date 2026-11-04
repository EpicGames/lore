// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Local-only transport diagnostic; no repository data or credentials.
import { spawn } from "node:child_process";
import { setTimeout as delay } from "node:timers/promises";

const worker = spawn("wrangler", ["dev", "--config", "wrangler.toml", "--local", "--ip", "127.0.0.1", "--port", "8788"], { stdio: ["ignore", "pipe", "pipe"] });
let output = "";
let exited = false;
let startupError;
worker.stdout.on("data", chunk => { process.stdout.write(chunk); output += chunk; });
worker.stderr.on("data", chunk => process.stderr.write(chunk));
worker.on("exit", () => { exited = true; });
worker.on("error", error => { startupError = error; exited = true; });
let status = 1;
try {
  for (let i = 0; i < 120 && !output.includes("Ready on http://127.0.0.1:8788"); i++) {
    if (exited) throw startupError ?? new Error("Wrangler exited before becoming ready");
    await delay(1000);
  }
  if (!output.includes("Ready on http://127.0.0.1:8788")) throw new Error("Wrangler startup timed out");
  const probe = spawn("cargo", ["run", "--locked", "--manifest-path", "../Cargo.toml", "--example", "tonic_probe", "--", "http://127.0.0.1:7000"], { stdio: "inherit" });
  status = await new Promise((resolve, reject) => { probe.on("error", reject); probe.on("exit", code => resolve(code ?? 1)); });
  if (status !== 0) throw new Error(`tonic probe exited ${status}`);
} finally {
  worker.kill("SIGTERM");
  process.exitCode = status;
}
