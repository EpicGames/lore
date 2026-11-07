---
type: fix
---

- `lore-client`: Fixed `lore service start` failing with exit code 32 when no service executable is named. It starts
  the service from the `lore` that ran it, as the configuration reference describes. A command that the service
  carries out because `[service] use_automatically` is on still starts one only from a named executable
