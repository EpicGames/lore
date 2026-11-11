---
status: accepted
date: 2026-07-29
amended: 2026-11-11
deciders: Raghav Narula
---

# ADR-00021: View-independent merges, subtree adoption, and out-of-view conflicts

## Context and Problem Statement

A `branch merge` in a checkout with a sparse view filter computed its changeset with that filter applied, so changes at paths outside the view never entered the merge. The merge still recorded the other branch as a parent, so the result claimed work it had dropped, and the branch stayed divergent from the merged branch at every out-of-view path.

The bug rests on a question the code had never answered: does a view filter scope the whole operation, or only the work that reaches disk? The two answers produce different merged trees for the same two branches, so the answer has to be written down.

Answering it raises two more. If the view gates disk work only, then a subtree outside the view never reaches the working tree, so merging it is a tree operation and may be cheaper than merging it file by file. And a conflict at such a path cannot be handled the usual way, because the `~mine`, `~theirs` and `~base` copies have no file to sit beside and no one can edit a merge result that is not on disk.

## Considered Options

What a view filter scopes:

- Scope tree work by the view and accept a partial merge
- Scope only on-disk work by the view, and always merge the full tree
- Refuse to merge in a sparse checkout

How to treat an untouched out-of-view subtree:

- Merge every out-of-view path file by file
- Adopt the subtree by discarding this branch's copy and rebuilding it
- Adopt the subtree by reconciling it against the other branch

A conflict at an out-of-view path:

- Keep this branch's side
- Adopt the incoming side, and record which side was taken
- Handle it as any other conflict: list it, show it in status, block the commit, and require `merge resolve`

## Decision Outcome

**The view filter gates on-disk work and never tree work.** Nodes merge regardless of the view; their file data is not materialised in a sparse working tree. Three parts:

- The merge diffs over a filter-free repository context, so out-of-view nodes enter the changeset.
- Realization gates every filesystem side effect for a node on the view, while still staging the node.
- A commit of a view-excluded staged file adopts the staged content hash rather than re-fragmenting a working-tree file that is absent or stale. **Amended 2026-11-11:** that holds for a file a merge staged. A forced stage takes a file past the view from the working tree, and the commit reads it there like any other staged file. A forced stage of a file the merge staged resolves the merge with the working tree's file, as an edit of a merged file in view does: it drops the node's content hash, taking the working tree's size, keeping the file ID and the merge bits. A zero hash with a nonzero size is content not yet stored, the state a newly staged add has, so the commit reads the file from the working tree and records it as merged. A zero hash with size 0 is an empty file, which a merge stages that way and the commit takes from the staged node as it stands.

**An out-of-view subtree the branch never touched is adopted by reconciling it.** `GraftOracle` requires the view to exclude every path below the directory, and the branch's version of it to be byte-identical to the base's. It refuses a path resolving into a linked repository. The walk then emits one `FileAction::Graft` change instead of descending, and the apply diffs the staged subtree against source's: a child whose address matches is skipped without descending, and a differing file is overwritten in place. **Amended 2026-10-30:** an added node is created, and an add and a delete of one file pair into a move by the file's identity, through the pairing the diff of a merge with no view uses. Each add, move, overwrite and delete takes the staged flags a per-file merge gives it, the ancestors of an add or a move included, so the commit discards the deletes, recomputes every directory the reconcile changed, and records the grafted changes in the merge's delta as a merge with no view does. Both record an added directory as an add, empty or not. A link the subtree deletes leaves the link registry before any link is added, so a link moved to a new path and pin is recorded at its new path. A move across the subtree's boundary is recorded as an add and a delete: the diff that pairs moves sees only the side outside the subtree. **Amended 2026-11-10:** the reconcile treats a link as a node and never descends into it. A link pinning another revision of the subtree it mounts is left to the merge's pass over the link pins, as in a merge with no view. Any other change to a link, one to a link the merge cannot read included, overwrites it whole. A type change is a delete of the staged node and an add of the other kind beside it, as staging one is.

`FilterInstance::excludes_subtree` answers whether the whole subtree is out of view. Testing the directory path alone is not enough, because a view is an ordered glob list and a later inclusion line can re-include part of an excluded area. **Amended 2026-10-30:** it requires the view to exclude the directory or one of its ancestors, and no path inclusion line at or after the line deciding that exclusion to be able to match below that path. A name inclusion such as `!keep.txt` never applies below an excluded directory. The filter's re-inclusion index bounds each path inclusion by its literal prefix and by the most path components it can match, so under `*`, `!/*`, `!a/b/**` the subtree `c/d` is out of view. **Amended 2026-10-30:** in place of the literal prefix, the index compares each leading component of a path inclusion with the path's as the matcher compares one, up to the first it cannot compare, such as `**` or a brace group, so under `**`, `!*/Saved/Config/` the subtree `Game/Saved/Logs` is out of view too.

**A conflict at an out-of-view path adopts the incoming side.** The node is staged from the source revision and flagged `StagedMergeTheirs`, the flag `merge resolve theirs` sets, so the choice is recorded rather than implied. Nothing is written to the working tree, and the path counts as merged rather than conflicted. An in-view conflict is unaffected.

### Consequences

- Good, because a merge produces the same tree whatever the local view is.
- Good, because an out-of-view path merges into the tree and stays off disk, so a sparse working tree stays sparse and stays correct.
- **Amended 2026-10-30:** Good, because a large merge under a view takes less memory the more of it is out of view: 60% less peak memory on a 110,000-file measurement. Its time stays within the variation between runs of the same merge with adoption disabled.
- Good, because reconciling costs work proportional to the change, not to the size of the subtree, and leaves every unchanged node exactly as a file-by-file merge would.
- Bad, because the merge carries two filters with opposite meanings. The walk gets a filter-free context and the real view is passed separately. No type distinguishes them, so a later change that picks the wrong one restores the original bug with no error.
- Bad, because a commit works out again that a file was never written, in a later command than the merge that decided it. Narrowing the view in between breaks the pair. Recording it on the node needs a new `NodeFlags` bit and all sixteen are in use.
- **Amended 2026-10-30:** Bad, because a move across an adopted subtree's boundary is recorded as an add and a delete under a view and as a move with no view, so the merge's delta and the moved file's history depend on the view, though its tree does not.
- Bad, because an out-of-view conflict is resolved without asking. It discards a version the branch had picked up by merging a third branch, and nothing reports that.
- Bad, because a conflict is then treated differently depending on the local view, so two people merging the same branches can produce different trees. The first decision removes that for ordinary changes; this reintroduces it for conflicts.

## More Information

Validate any change here by measurement, not by reading the code. Merge the same two branches twice on one repository, once under a sparse view and once with no view, and compare the staged merged-tree hashes. Equal hashes mean the view changed nothing about the result. Adoption failed that check in its first form and passes now, on several fixtures including one of 110,000 files with 100,000 changes.

**Amended 2026-10-30:** the figures in this paragraph come from a separate fixture of the same shape. They supersede the 152 s and 16.1 GiB first recorded here, and the two sets are not comparable. The fixture holds 10,000 files in view and 100,000 out of view, and the merged branch modifies 59,400, adds 19,800, deletes 19,800 and moves 1,000 of them. On release builds, three merges under the view take 17.0 to 18.4 s and 192 to 201 MiB at peak, and three with adoption disabled take 15.4 to 16.6 s and 495 to 521 MiB. The control merge, with no view, takes 20.6 s and 510 MiB. Every merge leaves the control merge's tree. Pairing moves takes 3 ms over the reconcile's 101,022 changes, and 5 to 7 ms over the same number in the diff of a merge that adopts nothing. Other builds shared the machine, so the times vary between runs by around 10%; the memory figures and the trees do not.

One trap, because the bit layout hides it: `NodeFlags::StagedMergeResolved` contains the `StagedMergeConflict` bits, so setting "resolved" also sets "conflicted". An adopted node marked resolved reads as a conflict, and the commit then looks for conflict markers in a file a sparse working tree does not have. `StagedMergeTheirs` does not carry those bits and is the correct flag on its own.

Two known gaps. `merge resolve theirs` does nothing at an out-of-view path, and such a path is absent from both the merge's conflict list and `lore status`. **Amended 2026-11-11:** `lore status` lists it as a staged change with the incoming side taken ([ADR-00027](00027-ignore-filter-applies-to-untracked-paths-only.md)). Adopting the incoming side means a merge no longer depends on either, but the conflict list and `merge resolve theirs` remain broken for anything else reaching a conflict there.

Expressing the walk scope through `FilterMode` instead of a second filter argument does not work today, because `revision::diff3` already uses the view slot for a path scope derived from the source changes. Splitting realization into a tree pass and a materialization pass would remove the two-filter problem, and is the shape to aim for if this area is revisited.
