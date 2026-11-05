---
type: improvement
---

- `lore-storage`: `WriteOptions` gains `keep_local`, with `with_keep_local` and `cache_local`, so a struct literal
  names it: the write keeps its payload in the writing store once it is durable on the remote, as `local_cache_priority`
  does, without recording it in the fragment for every store that fetches it to cache
- SWFS: an instance keeps the content it commits in its local store when the commit uploads it, so a sync back to that
  revision serves the files, where the mount served them as zeros once the store had dropped the uploaded payloads.
  Content writes follow the repository's caching setting, as reads do, so a repository run with `--cache` keeps what
  it uploads as well
