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
3. Start lore-web (`npm start`), click the **⚙** button beside the `lore web`
   logo, and enter the host's server URL (`lore://<host>:41337`). Click **Search
   again** first — lore-web checks common local addresses and lists any it finds,
   so you may only need to select one. Without this step, lore-web defaults to a
   local server address that will not reach the host's machine.
4. Clone the host's repository: click **Server repositories…** to browse the
   host's repositories, then **Clone** the one you want and pick a destination
   folder. (Already-cloned repos are tagged, and each row's ✕ deletes that
   repository from the server.) To clone a known URL directly instead, use
   **Clone from URL…**.
5. Work normally. Use **Push** to send commits to the host and **Sync** to pull
   the host's latest revision. Progress streams live in the dialog.

## Change a repository's organization

A repository's organization is the `org/` prefix of its name (the `acme` in
`acme/widgets`), shown as a pill beside the branch and as a badge in the sidebar.

1. Select the repository, then click the organization pill (it reads
   **Set organization…** when the name has no prefix yet).
2. Enter the new organization and confirm.

> [!CAUTION]
> The organization is part of a repository's read-only identity, fixed when the
> repository is created. Changing it rebuilds the local repository — preserving its
> id and remote but discarding local committed revisions. Push everything to the
> remote first, and only change the organization when you accept that local
> history loss.

## Result

Both machines drive the same repository: the host serves it, the collaborator
clones and pushes to it, and each side's lists refresh live as the other pushes.

## See also

- [HTTP API reference](../reference/http-api.md)
- [Architecture](../explanation/architecture.md)
- Lore CLI: [authentication](https://epicgames.github.io/lore/)
