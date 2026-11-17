---
type: breaking
---

- `lore`: `lore_service_status_event_data_t` gains seven fields after its existing ones: the path of the caller's external directories, whether they can be used (`lore_external_root_state_t`), the number of old directories left to remove, whether the service runs inside another program, the global data directories and paths of the virtual instances it mounts, and its process ID. A program that reads the event rebuilds against the new `lore.h`. A client and a service exchange the event, so upgrade them together.
- `lore-client`: `lore service status` prints the service's process ID, whether it runs inside another program, and the virtual instances it mounts with their global data directories. It prints the external directories, their state and the number of old directories to remove, also when no service is running.
- `lore-revision`: A service that mounts, creates, clones, deletes or moves a virtual instance holds `external.lock` in that global data directory, shared, until it exits. Where it can't take that lock, as while a move of the external directories holds it, a call that needs it fails, and a starting service mounts no virtual instance and logs why.
- `lore`: `lore service start` asks the service for its status once one is reachable, and prints the messages the service kept while no command ran, as `lore service status` does, at their log level. Those messages are then no longer kept for the next `lore service status`.
