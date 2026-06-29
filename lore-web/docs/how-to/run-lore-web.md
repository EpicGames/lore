# Run lore-web

Use this guide to start lore-web on your own machine, and to set up a
collaborator who syncs with your server but runs no server of their own.

## Before you start

- Node.js 18–24 is installed.
- You can reach the npm registry to install the SDK (one time).
- For collaborators: the `lore` CLI is on `PATH` and this machine can reach the
  host's server over the network.

## Run it on the host

1. Install dependencies:
   ```sh
   cd lore-web
   npm install
   ```
2. Start the app:
   ```sh
   npm start
   ```
   Your browser opens `http://127.0.0.1:7420`.
3. Click **Add**, paste the path to a Lore working copy (a folder containing a
   `.lore` directory), and select it.

## Set up a collaborator (no server)

1. Send the `lore-web` folder to the collaborator (it is self-contained — they do
   not need to clone the repository). They run `setup.bat` (or `npm install`) once.
2. If the host's server requires authentication, sign in once against it in a
   terminal (servers with no auth configured can skip this):
   ```sh
   lore login lore://<host>:41337
   ```
   This stores an identity the SDK reuses for `clone`, `sync`, and `push`.
3. Start lore-web (`npm start`) and clone the host's repository: click
   **Server repositories…** to browse the host's repositories, then **Clone** the
   one you want and pick a destination folder. (Already-cloned repos are tagged, and
   each row's ✕ deletes that repository from the server.) To clone a known URL
   directly instead, use **Clone from URL…**.
4. Work normally. Use **Push** to send commits to the host and **Sync** to pull
   the host's latest revision. Progress streams live in the dialog.

## Result

Both machines drive the same repository: the host serves it, the collaborator
clones and pushes to it, and each side's lists refresh live as the other pushes.

## See also

- [HTTP API reference](../reference/http-api.md)
- [Architecture](../explanation/architecture.md)
- Lore CLI: [authentication](https://epicgames.github.io/lore/)
