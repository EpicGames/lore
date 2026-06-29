// Thin wrapper over @lore-vcs/sdk (koffi FFI -> lorelib). All Lore work in
// lore-web flows through here so the FFI lifecycle and the failure contract are
// handled in exactly one place.
//
// Failure contract (per Lore's error-handling standard, errors.md §4): a verb's
// outcome is canonical on its COMPLETE event — `status` (0 = success, non-zero =
// FFI code) and an `error` detail (`errorCode`, `message`, `traceLocations`).
// Older builds instead emit a mid-stream ERROR event (`errorType`, `errorInner`);
// we read COMPLETE.error first and fall back to ERROR events, then raise a single
// typed LoreVerbError. Event-detail strings are read inside the iteration (the
// events are already cloned by the SDK), never retained past it.

import { lore, LoreError } from "@lore-vcs/sdk";
import { LoreEventTag, LoreLogLevel } from "@lore-vcs/sdk/types/enums";
import { log } from "./log.mjs";

/** A normalized Lore event: numeric tag resolved to its name plus the payload. */
/** @typedef {{ tag: string, tagRaw: number, data: any }} LoreEvt */

/** Raised when a Lore verb completes with a non-zero status. */
export class LoreVerbError extends Error {
  /** @param {string} message @param {{status?: number, code?: number, verb?: string}} [info] */
  constructor(message, info = {}) {
    super(message);
    this.name = "LoreVerbError";
    this.status = info.status ?? -1;
    this.code = info.code ?? info.status ?? -1;
    this.verb = info.verb;
  }
}

const TAG_NAME = /** @type {Record<number, string>} */ (LoreEventTag);

/** Resolve a numeric event tag to its enum name (falls back to the number). */
function tagName(raw) {
  return TAG_NAME[raw] ?? String(raw);
}

/** @param {any} ev */
function normalize(ev) {
  return { tag: tagName(ev.tag), tagRaw: ev.tag, data: ev.data };
}

/** Map SDK LOG severity onto our logger levels. */
const LOG_LEVEL = {
  [LoreLogLevel.TRACE]: "trace",
  [LoreLogLevel.DEBUG]: "debug",
  [LoreLogLevel.INFO]: "info",
  [LoreLogLevel.WARN]: "warn",
  [LoreLogLevel.ERROR]: "error",
};

/**
 * Look up a verb function on the SDK, failing loudly for unknown names rather
 * than letting an undefined call throw an opaque TypeError.
 * @param {string} verb
 */
function resolve(verb) {
  const fn = /** @type {any} */ (lore)[verb];
  if (typeof fn !== "function") {
    throw new LoreVerbError(`Unknown Lore verb: ${verb}`, { verb });
  }
  return fn;
}

/** Pull a readable message out of the ERROR events the SDK collected. */
function messageFromErrors(errors, fallback) {
  for (const e of errors ?? []) {
    const data = e?.data ?? e;
    const inner = data?.errorInner ?? data?.message;
    if (inner) return String(inner);
  }
  return fallback;
}

/**
 * Run a Lore verb to completion and return all of its events. Throws a
 * LoreVerbError if the operation fails.
 * @param {string} verb such as "revisionHistory"
 * @param {Record<string, unknown>} globalArgs at minimum `{ repositoryPath }`
 * @param {Record<string, unknown>} [args] verb-specific arguments
 * @returns {Promise<LoreEvt[]>}
 */
export async function collect(verb, globalArgs, args = {}) {
  const fn = resolve(verb);
  const events = [];
  let status = 0;
  /** @type {{message?: string, errorCode?: number}|null} */
  let complete = null;
  /** @type {string|null} */
  let errorEvent = null;
  try {
    for await (const ev of fn(globalArgs, args).asyncIter()) {
      const n = normalize(ev);
      if (n.tag === "COMPLETE") {
        status = n.data?.status ?? 0;
        complete = n.data?.error ?? null;
      } else if (n.tag === "ERROR") {
        errorEvent = n.data?.errorInner ?? errorEvent;
      }
      events.push(n);
    }
  } catch (err) {
    // asyncIter throws LoreError on a non-zero return; the COMPLETE detail
    // captured above is the canonical source, so mark failure and move on.
    if (!(err instanceof LoreError)) throw err;
    status = status || -1;
  }
  if (status !== 0) {
    const message = complete?.message || errorEvent || `Lore verb '${verb}' failed`;
    log.debug("lore verb failed", { verb, status, message });
    throw new LoreVerbError(message, { verb, status, code: complete?.errorCode ?? status });
  }
  return events;
}

/**
 * Stream a Lore verb's events as they arrive, for live progress over SSE. Yields
 * normalized events (including LOG, PROGRESS, and any ERROR detail) and a final
 * `{ tag: "DONE", data: { ok, status } }` marker. Never throws to the caller;
 * failures arrive as the terminal marker so the SSE channel can close cleanly.
 * @param {string} verb
 * @param {Record<string, unknown>} globalArgs
 * @param {Record<string, unknown>} [args]
 * @returns {AsyncGenerator<LoreEvt>}
 */
export async function* stream(verb, globalArgs, args = {}) {
  const fn = resolve(verb);
  let status = 0;
  /** @type {string|null} */
  let failure = null;
  const exec = fn(globalArgs, args);
  try {
    for await (const ev of exec.asyncIter()) {
      const n = normalize(ev);
      if (n.tag === "COMPLETE") {
        status = n.data?.status ?? 0;
        failure = n.data?.error?.message || failure;
      }
      if (n.tag === "ERROR") failure = n.data?.errorInner ?? failure;
      if (n.tag === "LOG") {
        const lvl = LOG_LEVEL[n.data?.level] ?? "debug";
        log[/** @type {"debug"} */ (lvl)](`lore: ${n.data?.message ?? ""}`, { verb });
      }
      yield n;
    }
  } catch (err) {
    if (err instanceof LoreError) {
      failure = messageFromErrors(err.loreErrors, failure);
      status = status || -1;
    } else {
      failure = err instanceof Error ? err.message : String(err);
      status = -1;
    }
  }
  const ok = status === 0 && !failure;
  yield { tag: "DONE", tagRaw: -1, data: { ok, status, message: failure ?? undefined } };
}

let configured = false;

/** Configure SDK file logging once. Safe to call repeatedly. */
export function configureSdk() {
  if (configured) return;
  configured = true;
  try {
    /** @type {any} */ (lore).logConfigure?.({
      file: false,
      level: LoreLogLevel.INFO,
      categories: 0,
    });
  } catch (err) {
    log.warn("sdk logConfigure failed", { error: err instanceof Error ? err.message : String(err) });
  }
}

/** Release the native library. Call on process shutdown. */
export function shutdownSdk() {
  try {
    /** @type {any} */ (lore).shutdown?.();
  } catch {
    // The process is exiting; a failure to release the lib is not actionable.
  }
}
