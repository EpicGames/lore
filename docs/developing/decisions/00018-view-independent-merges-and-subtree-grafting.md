---
status: accepted
date: 2026-07-29
deciders: Raghav Narula
---

# ADR-00018: View-independent merges, subtree adoption, and out-of-view conflicts

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
- A commit of a view-excluded staged file adopts the staged content hash rather than re-fragmenting a working-tree file that is absent or stale.

**An out-of-view subtree the branch never touched is adopted by reconciling it.** `GraftOracle` requires the view to exclude every path below the directory, and the branch's version of it to be byte-identical to the base's. It refuses a path resolving into a linked repository. The walk then emits one `FileAction::Graft` change instead of descending, and the apply diffs the staged subtree against source's: a child whose address matches is skipped without descending, a differing file is overwritten in place, an added node is created, a removed one is discarded.

`FilterInstance::excludes_subtree` answers whether the whole subtree is out of view. Testing the directory path alone is not enough, because a view is an ordered glob list and a later inclusion line can re-include part of an excluded area. It requires no inclusion line anywhere in the view, and some exclusion line of the form `<prefix>/**` covering the path.

**A conflict at an out-of-view path adopts the incoming side.** The node is staged from the source revision and flagged `StagedMergeTheirs`, the flag `merge resolve theirs` sets, so the choice is recorded rather than implied. Nothing is written to the working tree, and the path counts as merged rather than conflicted. An in-view conflict is unaffected.

### Consequences

- Good, because a merge produces the same tree whatever the local view is.
- Good, because an out-of-view path merges into the tree and stays off disk, so a sparse working tree stays sparse and stays correct.
- Good, because a large merge under a view gets cheaper the more of it is out of view: 34% less time and 29% less peak memory on a 110,000-file measurement.
- Good, because reconciling costs work proportional to the change, not to the size of the subtree, and leaves every unchanged node exactly as a file-by-file merge would.
- Bad, because the merge carries two filters with opposite meanings. The walk gets a filter-free context and the real view is passed separately. No type distinguishes them, so a later change that picks the wrong one restores the original bug with no error.
- Bad, because a commit works out again that a file was never written, in a later command than the merge that decided it. Narrowing the view in between breaks the pair. Recording it on the node needs a new `NodeFlags` bit and all sixteen are in use.
- Bad, because an out-of-view conflict is resolved without asking. It discards a version the branch had picked up by merging a third branch, and nothing reports that.
- Bad, because a conflict is then treated differently depending on the local view, so two people merging the same branches can produce different trees. The first decision removes that for ordinary changes; this reintroduces it for conflicts.

## More Information

Validate any change here by measurement, not by reading the code. Merge the same two branches twice on one repository, once under a sparse view and once with no view, and compare the staged merged-tree hashes. Equal hashes mean the view changed nothing about the result. Adoption failed that check in its first form and passes now, on several fixtures including one of 110,000 files with 100,000 changes.

On that fixture, against the same code with adoption disabled, merge time fell from 152.3 s to 100.2 s and peak memory from 16.1 GiB to 11.4 GiB, with the same tree hash on both sides. The control merge, which has no view and adopts nothing, did not change. It varies by around 10% between runs, so treat the time figure as approximate; the memory figure and the tree hash do not vary.

One trap, because the bit layout hides it: `NodeFlags::StagedMergeResolved` contains the `StagedMergeConflict` bits, so setting "resolved" also sets "conflicted". An adopted node marked resolved reads as a conflict, and the commit then looks for conflict markers in a file a sparse working tree does not have. `StagedMergeTheirs` does not carry those bits and is the correct flag on its own.

Two known gaps. `merge resolve theirs` does nothing at an out-of-view path, and such a path is absent from both the merge's conflict list and `lore status`. Adopting the incoming side means a merge no longer depends on either, but both remain broken for anything else reaching a conflict there.

Expressing the walk scope through `FilterMode` instead of a second filter argument does not work today, because `revision::diff3` already uses the view slot for a path scope derived from the source changes. Splitting realization into a tree pass and a materialization pass would remove the two-filter problem, and is the shape to aim for if this area is revisited.
