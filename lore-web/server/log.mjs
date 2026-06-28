// Structured logger following Lore's level discipline (Trace/Debug/Info/Warn/Error).
// Info is the default. Levels and ordering mirror lore-base/src/log/mod.rs so that
// SDK LOG events can pass through with their level preserved.

/** @typedef {"trace"|"debug"|"info"|"warn"|"error"} Level */

const ORDER = { trace: 1, debug: 2, info: 3, warn: 4, error: 5 };

const envLevel = (process.env.LORE_WEB_LOG_LEVEL || "info").toLowerCase();
const threshold = ORDER[/** @type {Level} */ (envLevel)] ?? ORDER.info;

/**
 * Emit one structured log line. Fields are rendered as `key=value` pairs after
 * the message rather than interpolated into it, per the logging standard.
 * @param {Level} level
 * @param {string} message
 * @param {Record<string, unknown>} [fields]
 */
function emit(level, message, fields) {
  if (ORDER[level] < threshold) return;
  const ts = new Date().toISOString();
  let line = `${ts} ${level.toUpperCase().padEnd(5)} ${message}`;
  if (fields) {
    for (const [k, v] of Object.entries(fields)) {
      const val = typeof v === "string" ? v : JSON.stringify(v);
      line += ` ${k}=${val}`;
    }
  }
  const sink = level === "error" || level === "warn" ? process.stderr : process.stdout;
  sink.write(line + "\n");
}

export const log = {
  /** @param {string} m @param {Record<string,unknown>} [f] */
  trace: (m, f) => emit("trace", m, f),
  /** @param {string} m @param {Record<string,unknown>} [f] */
  debug: (m, f) => emit("debug", m, f),
  /** @param {string} m @param {Record<string,unknown>} [f] */
  info: (m, f) => emit("info", m, f),
  /** @param {string} m @param {Record<string,unknown>} [f] */
  warn: (m, f) => emit("warn", m, f),
  /** @param {string} m @param {Record<string,unknown>} [f] */
  error: (m, f) => emit("error", m, f),
};
