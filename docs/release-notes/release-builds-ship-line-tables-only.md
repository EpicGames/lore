---
type: improvement
---

- `build`: The `-debug` archive beside each release archive is smaller, as it carries line tables
  only. Backtraces, crash reports and profilers that use it still show functions, files and lines;
  debuggers no longer show variables or types. Building with `--profile release-lto` needs less
  than half the memory.
