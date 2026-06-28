// Spike: prove the vendored @lore-vcs/sdk loads and runs from plain Node (no Electron).
// Usage: node server/smoke.mjs "<repositoryPath>"
import { lore, LoreError } from "@lore-vcs/sdk";

const repositoryPath = process.argv[2];
if (!repositoryPath) {
  console.error('Usage: node server/smoke.mjs "<repositoryPath>"');
  process.exit(2);
}

/** Run a Lore verb and return its collected events, surfacing failures. */
async function run(label, fn, globalArgs, args) {
  process.stdout.write(`\n# ${label}\n`);
  try {
    const events = await fn(globalArgs, args).collectAsync();
    for (const e of events) {
      const data = e.data ? JSON.stringify(e.data) : "";
      console.log(`  [${e.tag}] ${data.slice(0, 200)}`);
    }
    return events;
  } catch (err) {
    if (err instanceof LoreError) {
      console.error(`  LoreError:`, err.loreErrors?.map?.((x) => x?.data ?? x) ?? err);
    } else {
      console.error(`  threw:`, err);
    }
    return null;
  }
}

console.log(`SDK loaded OK. Target repo: ${repositoryPath}`);

await run(
  "revisionHistory (length 5)",
  lore.revisionHistory,
  { repositoryPath },
  { length: 5 },
);

await run(
  "repositoryStatus",
  lore.repositoryStatus,
  { repositoryPath },
  { staged: false },
);

// The native lib keeps the process alive; release it explicitly.
if (typeof lore.shutdown === "function") lore.shutdown();
console.log("\nDone.");
