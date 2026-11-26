---
type: feature
---

- `lore-revision`: A FUSE virtual instance serves a repository link read-only with the content of
  the revision it pins, as an SWFS virtual instance does, nested links included: the pin the staged
  state holds, or the current revision's where nothing is staged. A link whose pinned revision
  can't be read is served empty, and the service warns once for it, the first time something in
  it is looked up or it is listed; it reads the revision again on each later lookup or listing,
  so the link's content shows once the revision can be read. A write at or below a link is
  refused as read-only.
- `lore-revision`: On a FUSE virtual instance, a merge bringing a link, `lore link update`,
  `lore link remove` and a reset undoing a link change show at once, and a sync or branch switch
  moving a link to another pin serves the new pin, without the service mounting the instance
  again. A link that would replace local changes is refused unless forced, which discards them.
