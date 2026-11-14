---
type: fix
---

- `lore-revision`: Fixed pushing a branch created over nested links failing because `lore branch create` made the branch
  only in the outermost linked repositories rather than at every level. A repository that several links reach gets the
  branch once, at the newest of their pins. Where those pins are on lines that diverged, the create is refused, also in
  a dry run, and names both links
- `lore-revision`: Fixed a `lore branch create` that failed in some linked repositories, or while storing the new
  branch, leaving the branch in the other linked repositories. It deletes them, fails with the first failing
  repository's error, such as its refusal, and names every link that failed
- `lore-revision`: Fixed the revision `lore branch create` writes to repin links taking the message of the revision it
  was created from. It reads `Repin links onto branch <name>`
- `lore-revision`: Fixed restoring a deleted branch with `lore branch create --id` leaving the working tree at the
  current revision rather than at the revision the branch left off at
