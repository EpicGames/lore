---
type: feature
---

- `lore-client`: `lore service set-external-directory <path>` moves every virtual instance's external directory to a new external root on the volume that holds the current one, and records it in `external-root` in the global data directory. It stops the running service, holds its socket while the external root moves, starts the service again, and leaves a link at the default location for older versions. A move to another volume is refused. `--adopt` points the setting at a copy of the external root, or writes a lost `external-root` again, and `--abandon` gives up an external root on a failed volume for a new, empty one. `--dry-run` reports what the move would do, and Ctrl-C before the final switch undoes the move. See the how-to page Move the external directories of virtual instances.
- `lore-capi`: `lore_service_set_external_directory` and `lore_service_set_external_directory_async` run the same command, with `lore_service_set_external_directory_args_t`.
- `lore`: A starting service finishes or undoes a move of the external root that was interrupted, before it mounts anything, and removes the old directories a move left, in the background.
