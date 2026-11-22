---
type: improvement
---

- `lore-revision`: Under a view, `lore clone`, `lore sync`, `lore branch switch`, `lore sync --view`, `lore reset`, `lore layer add` and `lore link add` create a directory where the last view rule matching it re-includes it, as `!/Engine/Content/` does, where it holds at any depth something the command creates, or where the repository holds it empty. They no longer create a directory the view keeps only because its parent is kept, or one Lore looks into only because a re-inclusion could match below it, while nothing below it is kept. A view of extension rules now creates only the directories holding a kept file: of the 5.55 million directories a compile workspace view with 110 rules created, about 140 thousand hold one. `lore sync --view` removes such a directory once the files it held leave and it is empty, and a dry run of `lore clone` lists the directories the clone creates.
