// Fallback to the installed `lore` CLI for the few things better handled by the
// real process than the in-process SDK: interactive browser login and the
// service lifecycle. The SDK is the primary engine; this is the hybrid escape
// hatch. The CLI emits no machine-readable output, so callers treat results as
// status + text, not structured data.

import { spawn } from "node:child_process";
import { log } from "./log.mjs";

const LORE_BIN = process.env.LORE_CLI ?? "lore";

/**
 * Run a `lore` subcommand to completion, capturing its output.
 * @param {string[]} args CLI arguments, such as ["auth", "list"]
 * @param {{ repoPath?: string }} [opts]
 * @returns {Promise<{ code: number, stdout: string, stderr: string }>}
 */
export function runCli(args, opts = {}) {
  const full = opts.repoPath ? ["--repository", opts.repoPath, ...args] : args;
  return new Promise((resolve) => {
    const child = spawn(LORE_BIN, full, { windowsHide: true });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d));
    child.stderr.on("data", (d) => (stderr += d));
    child.on("error", (err) => {
      log.warn("lore cli spawn failed", { error: err.message });
      resolve({ code: -1, stdout, stderr: String(err.message) });
    });
    child.on("close", (code) => resolve({ code: code ?? -1, stdout, stderr }));
  });
}

/**
 * Report whether the CLI has any stored identity — whether the user has logged in.
 * @returns {Promise<boolean>}
 */
export async function isLoggedIn() {
  const { code, stdout } = await runCli(["auth", "list", "--no-pager"]);
  return code === 0 && /\S/.test(stdout) && !/no identities/i.test(stdout);
}
