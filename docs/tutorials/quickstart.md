# Quickstart: Your first Lore repository

Set up Lore on your machine and run through the core loop in under 10 minutes. By the end of this tutorial you'll have installed the Lore CLI, deployed a local Lore Server, created a repository, committed and pushed a revision, cloned the repository into a second working tree, created a branch and merged it back into main, and synced the merged result across both working trees.

## Prerequisites

- A terminal (macOS/Linux) or PowerShell (Windows).
- Ports 41337 and 41339 free on your machine.

You don't need a Rust toolchain, a cloned repository, or Docker for this tutorial. The install script fetches both prebuilt binaries for you.

## Step 1 — Install Lore and start a local server

Run Lore's install script in demo mode. It downloads the prebuilt `lore` CLI and `loreserver` binaries, puts them on your PATH, and starts a local server. With no configuration, the server starts a single-node instance, generating an ephemeral self-signed certificate and a temporary store under your system temporary directory.

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.sh | bash -s -- --demo
```

<!-- tab -->
**Windows**

```powershell
$env:LORE_DEMO=1; irm https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.ps1 | iex
```

<!-- tabs:end -->

The server keeps running in this terminal, listening on port 41337 for QUIC and gRPC and on port 41339 for HTTP. Open a new terminal for the rest of the tutorial, and leave this one alone — pressing Ctrl-C or closing it stops the server.

> [!IMPORTANT]
> Run with no config, the server generates an ephemeral self-signed certificate and a store under your system temporary directory (`<temp>/lore-server`). The certificate is untrusted and regenerated on every restart, and the temporary store isn't durable across reboots — both are fine for a throwaway tutorial server. To deploy a more persistent Lore server see [Deploy a local Lore Server](../how-to/deploy-local-lore-server.md) and for the full list of server configurables, see the [Lore Server config reference](../reference/lore-server-config.md#zero-config-defaults).

<!-- -->

> [!NOTE]
> Prefer a manual binary download, a custom PATH, or a build from source? See [Install the Lore CLI](../how-to/install-lore-cli.md).

## Step 2 — Verify server health and create a repository

In your new terminal, first confirm the server is healthy:

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
curl -i http://127.0.0.1:41339/health_check
```

<!-- tab -->
**Windows**

```powershell
curl.exe -i http://127.0.0.1:41339/health_check
```

<!-- tabs:end -->

A healthy server returns `HTTP/1.1 200 OK` with an empty body. The server starts with auth disabled — no credentials are needed for this tutorial.

Now create a repository:

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
mkdir ~/my-project && cd ~/my-project
```

<!-- tab -->
**Windows**

```powershell
mkdir ~/my-project
cd ~/my-project
```

<!-- tabs:end -->

```bash
lore repository create lore://127.0.0.1:41337/my-project
```

Expected output:

```text
Created repository my-project in /home/you/my-project with ID 3f2a1b4c5d6e7f8a...
```

(Your path and ID will differ. The ID is a 32-character hex string.)

Lore initializes the repository on the server and creates a working tree in the current directory. A `.lore/` directory appears alongside your files — that's where Lore keeps its local state. Inside `.lore/` is the client config, `.lore/config.toml` — the remote URL, your commit identity, and local store settings. See the [Lore CLI configuration reference](../reference/lore-cli-config.md#per-repository-configtoml) for what's recorded there.

## Step 3 — Add files and stage them

Create a text file:

```bash
echo "Hello, Lore" > hello.txt
```

Create a small binary file:

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
python3 -c "import os; open('sample.bin', 'wb').write(os.urandom(256))"
```

<!-- tab -->
**Windows**

```powershell
fsutil file createnew sample.bin 256
```

<!-- tabs:end -->

Stage both files:

```bash
lore stage hello.txt sample.bin
```

Confirm the staged state:

```bash
lore status --scan
```

Expected output (no revisions yet, so the revision number is 0 and its hash is all zeros):

```text
Repository 3f2a1b4c5d6e7f8a...
On branch main revision 0 -> 0000000000000000...
Remote revision 0 -> 0000000000000000...
Local branch in sync with remote
Changes staged for commit:
A hello.txt
A sample.bin
```

(Your repository ID will differ. The `A` prefix means the file is newly added.)

> [!NOTE]
> `lore stage` covers adds, edits, and deletes — you use the same command for all three. Stage a deleted file and Lore records the deletion for the next commit. Moves and renames are tracked too, through a dedicated subcommand: `lore stage move <from> <to>` records the rename so the file keeps its identity and history across the move instead of registering as a delete plus an add.

## Step 4 — Commit the revision

Record the staged files as a new revision:

```bash
lore commit "Initial revision"
```

Expected output:

```text
Fragmenting files and updating tree hashes
Committing staged changes
Committed 1/1 directories, 2/2 files, 269.00 bytes/269.00 bytes (2 modified, 0 deleted)
Repository: 3f2a1b4c5d6e7f8a...
Revision  : 1
Signature : a3f8c2d1...
Branch    : e7263180...
Date      : Wed, 14 Jan 2026 09:24:18 +0000
    Initial revision
Commit succeeded
```

(Your repository ID, hashes, and timestamp will differ.)

The revision is local until you push it. Staging and committing work fully offline — no server round-trip is needed for either step.

> [!NOTE]
> An unauthenticated server like this one won't record a commit author, but you can still keep a who-did-what history by setting your own identity. You can choose to set your identity in `.lore/config.toml` (seeded for you when you pass `--identity you@example.com` to `lore repository create` or `lore clone`), to ensure commits are attributed to you from then on. The demo skips this, so its commits record no author — fine for a throwaway run. See the [CLI configuration reference](../reference/lore-cli-config.md#identity) for more info.

## Step 5 — Push to the server

Upload the revision to the server:

```bash
lore push
```

Expected output:

```text
Pushing 1 fragment(s)
Pushed 1 fragment(s), 124.00 bytes
Pushing a3f8c2d1... to branch main
Pushed revision 1 -> a3f8c2d1... to branch main
```

(Your hashes and byte count will differ — the byte count is the size of the stored, compressed fragments, not the size of your files.)

> [!CAUTION]
> If `lore push` fails with a conflict error, another client pushed to the same branch since your last sync. Run `lore sync` to pull the remote changes, resolve any conflicts, commit the merge, then push again.

## Step 6 — Set up a shared store and clone a second working tree

Now that the repository has a revision on the server, clone it into a sibling directory to get a second, independent working tree. First, create a *shared store* for the server. A shared store lets you keep multiple worktrees on your machine without each one costing its own disk space: instead of copying content into every tree's `.lore/`, all the trees that point at the store draw from one on-disk copy. Any tree cloned from the same server can use it, and because Lore stores content as deduplicated *fragments*, even similar files share the parts they have in common — not just byte-for-byte duplicates.

Create the store, passing the server URL — host and port, with **no** repository path:

```bash
cd ~
lore shared-store create lore://127.0.0.1:41337
```

`lore shared-store info` shows where the store lives.

Now clone into the sibling directory, adding `--use-shared-store` so the new tree keeps its objects in the shared store instead of its own `.lore/`:

```bash
lore clone lore://127.0.0.1:41337/my-project my-project-b --use-shared-store
```

Expected output:

```text
Cloning repository 3f2a1b4c5d6e7f8a... branch main into /home/you/my-project-b
Pull state a3f8c2d1...
Cloned 2/2 files (269.00 bytes/269.00 bytes)
Branch main revision a3f8c2d1...
Clone complete in 0.12s
```

(Your repository ID, hashes, and timing will differ.)

The clone already contains the committed files — confirm it:

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
cd ~/my-project-b && lore status && ls
```

<!-- tab -->
**Windows**

```powershell
cd ~/my-project-b
lore status
ls
```

<!-- tabs:end -->

```text
Repository 3f2a1b4c5d6e7f8a...
On branch main revision 1 -> a3f8c2d1...
Remote revision 1 -> a3f8c2d1...
Local branch in sync with remote
hello.txt
sample.bin
```

You now have two independent working trees of the same repository.

Go back into the primary working tree:

```bash
cd ~/my-project
```

> [!CAUTION]
> If a step fails with a connection error, check that the server is still running: `curl -i http://127.0.0.1:41339/health_check` (`curl.exe` on Windows). If it's not responding, restart the `loreserver` process by re-running the install command from Step 1, then check the health endpoint again.

## Step 7 — Create a branch and commit on it

Create a branch from the current revision and switch your working tree to it:

```bash
lore branch create my-first-branch
```

Expected output:

```text
Created branch my-first-branch at revision a3f8c2d1...
```

Add a file on the branch, then stage and commit it:

```bash
echo "Notes added on a branch." > notes.txt
lore stage notes.txt
lore commit "Add notes on a branch"
```

Expected output:

```text
Fragmenting files and updating tree hashes
Committing staged changes
Committed 1/1 directories, 1/1 files, 25.00 bytes/25.00 bytes (1 modified, 0 deleted)
Stored history for 2 nodes
Repository: 3f2a1b4c5d6e7f8a...
Revision  : 2
Signature : b4e9f0a2...
Parent    : a3f8c2d1...
Branch    : 9a1c4b7d...
Date      : Wed, 14 Jan 2026 09:26:02 +0000
    Add notes on a branch
Commit succeeded
```

The branch now has a revision (2) that `main` doesn't. The `Parent` line points back at the revision you branched from.

## Step 8 — Merge the branch into main

Switch back to `main`. Lore updates your working tree to match `main`, so `notes.txt` disappears for now — it lives only on the `my-first-branch` branch until you merge it.

```bash
lore branch switch main
```

```text
Switching branch to main, using current remote latest revision a3f8c2d1...
Calculating deltas 2 -> 1
Verifying 1 changes with local file system
Switched to branch main revision a3f8c2d1...
```

Merge the branch into the current branch (`main`). `lore branch merge` takes the **source** branch as its argument:

```bash
lore branch merge my-first-branch --message "Merge my-first-branch into main"
```

Lore prints the diff it computes between the branches (omitted as `...` below), then the merge result. A clean merge with no conflicts commits a new revision automatically:

```text
Starting merge of branch 9a1c4b7d... revision b4e9f0a2...
...
Merged files, 1 updated, 0 deleted, 0 merged, 0 conflicted
Staged merged repository state 5e2a9f10...
Fragmenting files and updating tree hashes
Stored history for 3 nodes
Committed merged repository state 3 -> c7d1e8b3...
```

`main` is now at revision 3, with `notes.txt` merged in — but that revision is local. Check the status and push it to the server:

```bash
lore status
lore push
```

```text
Repository 3f2a1b4c5d6e7f8a...
On branch main revision 3 -> c7d1e8b3...
Remote revision 1 -> a3f8c2d1...
Local branch is ahead of remote
Local branch is 1 revision(s) ahead of remote, pushing all revisions
Repository 3f2a1b4c5d6e7f8a...
Pushing 6 fragment(s)
Pushed 6 fragment(s), 603.00 bytes
Pushing c7d1e8b3... to branch main
Pushed revision 3 -> c7d1e8b3... to branch main
```

(Your hashes, fragment count, and byte count will differ.)

> [!NOTE]
> If the merge reports conflicts instead, Lore stages the merge and lists the conflicted files rather than committing. Resolve them with `lore branch merge resolve`, then commit — or run `lore branch merge abort` to back out.

## Step 9 — Sync the second working tree

Switch to the clone you made in Step 6 and pull the merged revision from the server:

```bash
cd ~/my-project-b
lore sync
```

Expected output:

```text
Sync from remote lore://127.0.0.1:41337
On branch main revision 1 -> a3f8c2d1...
Synchronizing to revision 3 -> c7d1e8b3...
Calculating deltas 1 -> 3
Verifying 1 changes with local file system
```

Confirm the merged file arrived:

```bash
ls
```

```text
hello.txt
notes.txt
sample.bin
```

`notes.txt` — created on a branch in one working tree, merged into `main`, pushed, and synced into the other working tree — completes the loop.

## Verify

Run `lore status` in each working tree and confirm both are on `main` at revision 3, in sync with the server:

```bash
# In ~/my-project
lore status
```

```text
Repository 3f2a1b4c5d6e7f8a...
On branch main revision 3 -> c7d1e8b3...
Remote revision 3 -> c7d1e8b3...
Local branch in sync with remote
```

<!-- tabs:start -->

<!-- tab -->
**macOS / Linux**

```bash
# In ~/my-project-b
cd ~/my-project-b && lore status
```

<!-- tab -->
**Windows**

```powershell
# In ~/my-project-b
cd ~/my-project-b
lore status
```

<!-- tabs:end -->

```text
Repository 3f2a1b4c5d6e7f8a...
On branch main revision 3 -> c7d1e8b3...
Remote revision 3 -> c7d1e8b3...
Local branch in sync with remote
```

Your revision hash will differ from `c7d1e8b3...`. Both working trees at revision 3 with no staged changes, both in sync with the remote, means the core loop worked.

## Troubleshooting

**Server not reachable.** Run `curl -i http://127.0.0.1:41339/health_check` (`curl.exe` on Windows). A healthy server returns `HTTP/1.1 200 OK`. If it doesn't respond, the `loreserver` process has stopped — re-run the install command from Step 1 to start it again, then check the health endpoint again.

**Push rejected with a conflict.** If `lore push` reports the remote branch has moved, run `lore sync`, resolve any conflicts, commit, then push again.

**`python3` not found.** On macOS/Linux, if `python3` isn't installed try `python -c "..."` with the same arguments, or `head -c 256 /dev/urandom > sample.bin`. Any binary-format file works; copy any small image or compiled artifact you have on hand if you prefer.

## Next steps

- [Lore CLI command reference](../reference/lore-cli-commands.md) — every command you ran in this tutorial, with all its subcommands and flags.
- [Lore CLI configuration reference](../reference/lore-cli-config.md) — the `.lore/config.toml` created in Step 2, plus the user-level `cli.toml`.
- [Lore Server config reference](../reference/lore-server-config.md) — every server config field, its default, and how to point the server at a persistent store.
- [Install the Lore CLI](../how-to/install-lore-cli.md) — alternative PATH setups, shell completions, and building the CLI from source.
- [Deploy a local Lore Server](../how-to/deploy-local-lore-server.md) — stand up a persistent server with your own certificate and store, run it from source, or run it in Docker.
