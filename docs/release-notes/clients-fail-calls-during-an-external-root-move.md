---
type: breaking
---

- `lore`: While `lore service set-external-directory` moves the external root, the service socket answers every call with a new reply that names the move. A client of this version that relays its commands to the service then runs a command in an ordinary instance in its own process, and fails a command in a virtual instance with exit code 32. A client of an earlier version can't read the reply and fails the call. Upgrade every client and every program that links Lore on the machine before you move the external root.
