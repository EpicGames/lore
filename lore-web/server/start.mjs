// Launch the server (if not already running) and open it in the default browser.
// This is the everyday entry point (`npm start` / start.bat); use `npm run serve`
// to run headless without opening a browser.

import { spawn } from "node:child_process";
import { connect } from "node:net";

const host = process.env.LORE_WEB_HOST ?? "127.0.0.1";
const port = Number(process.env.LORE_WEB_PORT ?? 7420);
const url = `http://${host}:${port}`;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Resolve true if something is already listening on the target port. */
function isUp() {
  return new Promise((resolve) => {
    const sock = connect(port, host);
    sock.on("connect", () => {
      sock.destroy();
      resolve(true);
    });
    sock.on("error", () => resolve(false));
  });
}

/** Open the URL with the platform's default browser opener. */
function openBrowser() {
  const [cmd, args] =
    process.platform === "win32" ? ["cmd", ["/c", "start", "", url]]
    : process.platform === "darwin" ? ["open", [url]]
    : ["xdg-open", [url]];
  try {
    spawn(cmd, args, { detached: true, stdio: "ignore" }).unref();
  } catch {
    // Opening the browser is best-effort; the URL is printed below regardless.
  }
}

if (await isUp()) {
  // An instance is already running — open the browser to it.
  console.log(`lore-web is already running. Opening ${url}`);
  openBrowser();
} else {
  // Start the server, then wait for it to accept connections before opening.
  await import("./index.mjs");
  let ready = false;
  for (let i = 0; i < 50 && !ready; i++) {
    ready = await isUp();
    if (!ready) await sleep(100);
  }
  if (ready) {
    console.log(`\nlore-web is running. Opening ${url}`);
    console.log(`If your browser does not open, go to ${url} manually.\n`);
    openBrowser();
  } else {
    console.error(`Server did not become ready on ${url}. Check the log above.`);
    process.exit(1);
  }
}
