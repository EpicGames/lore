# How-To guides

Goal-driven directions for getting a specific task done.

## What this folder is

How-To guides are directions. Each one walks through a specific problem or toward a particular result. They assume you're already familiar with the basics. If you're still learning, start with tutorials.

## Guides

- [Install the Lore CLI](install-lore-cli.md) — get the `lore` binary onto your PATH and ready to use.
- [Deploy a local Lore Server](deploy-local-lore-server.md) — stand up a single-host Lore Server you can push to and clone from.
- [Run parallel instances over a shared store](run-parallel-instances-over-a-shared-store.md) — Lore's equivalent of worktrees: keep many working trees of a repository over one store, and move, prune, and remove them.
- [Use virtual clones on Linux](use-virtual-clones-on-linux.md) — clone a repository as a FUSE mount the Lore service serves, get your clones back after the service stops, and move and remove them.
- [Run a machine server for containers](run-a-machine-server-for-containers.md) — serve the containers of a build machine from one Lore server and one content cache, each container through a socket of its own.
- [Write view filters and ignore files](write-view-filters-and-ignore-files.md) — choose what reaches your working tree and what you can commit, with one rule syntax.

## Suggested starting points

- **Writing a new How-To?** Start at the [doc-standards walkthrough](../developing/doc-standards/writing-a-doc.md).
- [How-To template](how-to-template.md) — copy this when starting a new How-To guide.

See [docs/README.md](../README.md) for the full docs structure.
