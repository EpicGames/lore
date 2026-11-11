---
status: proposed
date: 2026-10-30
amended: 2026-11-11
---

# ADR-00027: The ignore filter applies only to paths the tree does not hold

## Context and Problem Statement

A repository has two path filters (`lore-revision/src/filter.rs`). The view (`.lore/view`) declares the sparse subset of the repository that is materialized on disk. The ignore file (`.loreignore`) declares paths that are not to be staged. Each operation declares which of the two it consults through a `FilterMode`. Before this decision, operations on the working tree and the staging area — staging, status, dirty marking, unstaging — consulted both, and asked the ignore file about every path whether or not the tree held it (`system-design.md` §12.4).

`--force` stages a path past the ignore file, and that is how an ignored file gets into the tree. Once it is there, every operation that asked the ignore file first lost sight of it. A report against 0.12.1 (`lukasz.jastrzebski/test_ignored_scan`) shows the result:

- `lore status` and `lore status debug.log` do not show a staged `debug.log`, because the comparison against the staged state filters with `FilterMode::Full`.
- `lore unstage .` leaves it staged, and the commit then takes it although the user can see nothing staged.
- Once committed, a change to `debug.log` is invisible to `status --scan` and `stage --scan`, and `lore dirty debug.log` does nothing. Staging a change takes `lore dirty --force` and then `lore stage --force`.

Revision 2044 ("List staged changes to ignored paths in lore status") fixed the first two: a `FilterMode::KeepStaged` bit stops the ignore file from excluding a node with something staged at or below it, in status and in unstage. It left the third as it was on purpose, keeping a change that is only marked dirty hidden on an ignored path, as a scan hides it. Revision 2066 ("Keep view-excluded directories only for the content below them") went further the same way for deletions: an unforced `stage` keeps a committed file the ignore file names when its directory leaves the working tree, and a committed empty directory the ignore file names that the working tree lacks.

The rest of the design record says the opposite. `system-design.md` §12.3 and the glossary say the ignore file is about new content only, and that a file already in the committed revision keeps flowing through until it is explicitly removed. Git behaves that way too: `.gitignore` has no effect on a tracked file. Should the ignore file be asked about a path the tree already holds?

## Decision Drivers

- **Nothing in the tree is hidden.** A path that will be committed must be visible to `status` and reachable by `unstage`. Hiding staged content is the worst outcome, because the commit takes what the user cannot see.
- **The ignore file keeps doing its job.** New files it names, at any depth, including untracked files below a tracked directory it names, must stay out of `status --scan` and `stage --scan` without `--force`.
- **The view is unchanged.** A sparse working tree never holds what the view leaves out, so the view has to go on filtering tracked paths as well as untracked ones.
- **One meaning for `--force`.** `--force` already puts a staging walk past both filters. Tracked content should not need it to flow through ordinary staging.
- **Least surprise.** Users coming from Git expect a tracked file to stay tracked whatever `.gitignore` says.

## Considered Options

- Ask the ignore file only about paths the tree does not hold.
- Keep asking it about every path except what is staged, as revision 2044 does: tracked ignored files stay reachable only through `--force`.
- Keep the behavior and document it.

## Decision Outcome

Chosen option: "Ask the ignore file only about paths the tree does not hold", because it is the only option that hides nothing that will be committed, matches §12.3, the glossary and Git, and needs no `--force` to keep a tracked file current.

A node is *tracked* for this purpose when it is staged, including a path staged past the ignore file with `--force`, or when the revision the tree was built on holds it (`State::tracks`). That is decided by the node, not by its path or its dirty flags: a tree shares its node IDs with the revision it was built on, so the revision holds a node where its own tree holds one of the same type at the same ID, which is how staging already tells a committed node from an uncommitted one. Everything else in the tree is only marked, and the ignore rules apply to it as to any untracked file:

- An add marked dirty names a file nobody has asked to add, so a rule added after `lore dirty` marked it drops it again, as Git drops an untracked file a new `.gitignore` rule names. The same goes for an add `lore dirty --force` marked, until it is staged, and for each new directory above it that the mark creates.
- A copy marked with `lore dirty copy` is a new node, so a copy into a directory the ignore file names is left out until it is staged.
- A move marked with `lore dirty move` keeps the committed node it moves, so it stays tracked at its new path, even inside a directory the ignore file names. Git differs: a rename it has not staged leaves the destination untracked. `lore dirty move` is an explicit statement that the file moved, of content already committed, so it is kept.

A new directory only marked dirty is recorded for staging only where it is empty; one holding nodes is carried by them, since staged or re-marked whole it would take in everything the working tree holds below it.

A path is tracked as the kind of node the tree holds there. Revision 2055 asks the filter about each path as the file or directory it is, and a directory-only rule such as `Intermediate/` names directories alone. Where the working tree holds a directory at a path the tree holds a file at, or the reverse, the working-tree entry is new content and answers to the ignore file, and the tree's node is reported and staged deleted like any other (`State::tracks_as`).

- **One filter bit.** Revision 2044's bit becomes `FilterMode::KeepTracked`, and its mode `FilterMode::STAGED` becomes `FilterMode::TRACKED`: the view, and the ignore rules for what the tree does not track. A walk clears the bit with `FilterMode::at` for each node the tree does not hold. The ignore slot is still stepped for a tracked node, so an untracked file below a tracked, ignored directory is still ignored. Whole-path queries honour the bit as well, since every ancestor of a tracked path is tracked.
- **Comparisons of trees.** The comparison against the staged state in `status`, and `unstage`, ask `at` whether something tracked is staged *or marked dirty* at or below a node (`State::holds_tracked_change`), where revision 2044 asked about staged alone. A node only the staged side holds, such as a move destination, is asked about by its node. Every node they reach is tracked, and a subtree with nothing changed in it has nothing to report, so an ignored one is still pruned without being entered, and a deleted ignored directory is still listed as one change. **Amended 2026-11-11:** the comparison in `status` keeps a node past the view where something is staged at or below it, in `FilterMode::CHANGES`: `TRACKED` with `FilterMode::KeepStagedInView`, which the walk steps and clears as it does `KeepTracked`, and clears as well for a node that holds nothing staged (`FilterMode::at_staged`). A forced stage takes content past the view, and a merge stages its changes at paths the view excludes ([ADR-00021](00021-view-independent-merges-and-subtree-grafting.md)). The commit takes both, so status lists both, and a path where the merge took the incoming side as a staged change with that side taken. A dirty mark past the view is not listed, as `status --scan` lists none, and a subtree the view excludes with nothing staged in it is still pruned without being entered. `unstage` and the walks over the working tree keep `TRACKED`, so a plain `unstage` still honours the view, and `lore --force unstage` takes back content a forced stage staged there outside a merge.
- **Walks over the working tree.** The filesystem diff (`status --scan`), the staging walk and the dirty walk look each entry up in the tree before asking the filter, and ask `TRACKED.at` whether the tree holds it. So do the path-level checks on a path the caller names, the lock commands' among them, including one the filter leaves out whatever it holds, which is dropped unread only where the tree does not track it. A path inside a layer is filtered where it is mounted, and looked up where the layer's trees hold it, below the layer's source.
- **Walks over the tree alone.** The collections of dirty paths ask `TRACKED.at` whether each node is tracked: the one `stage` without `--scan` or `--force` resolves a directory to, and the one a staged-state rebase (after a sync or a branch switch) and a commit replay onto the new revision, so neither drops a dirty mark on a tracked ignored file. The delete passes in the dirty and filesystem walks consult the view alone, since everything they delete is in the current revision.
- **`file info`.** The local and filtered sizes `lore file info` reports are measured against the staged tree, as `State::tracks` reads it for status, staging and locks, or against the revision `--revision` names. A path it names, and its walks over the working tree and over that tree, ask the ignore rules only about what that tree does not track, so a staged ignored file, and one `lore dirty move` moved into an ignored directory, count, and a named untracked file the rules name does not. The walk over the working tree reads that tree only where the rules would leave an entry out. Like the lock commands, it looks a path up through links. Neither looks a path inside a layer up in the layer's trees, so below a layer mount the ignore rules apply as to untracked content.
- **Deletions.** The staging walk's deletion of what the working tree lacks asks `TRACKED.at` too, so an unforced `stage` deletes a committed file the ignore file names along with its directory, and a committed empty directory the ignore file names, where revision 2066 kept them. The view still keeps what it excludes, and an empty directory it excludes, which nothing materializes.
- **Cost.** Every walk asks the filter without the tracked bit first, and asks the tree only where the ignore rules would leave a node out (`Filter::tracked_mode`). The revision a staged tree was built on is read once, apart from the tree, the first time it is asked, and kept on the tree for as long as it is built on that revision.
- **What does not change.** Operations on committed state already consult the view only.
- **Forcing.** `--force` puts a staging walk past both filters, and restages a node whose content is unchanged. A forced stage of a directory walks it on disk as `--scan` does, so `lore --force stage .` stages everything the working tree holds but Lore's own entries. Its deletions of what the working tree lacks still ask the view, since a sparse working tree lacks what the view leaves out on purpose.

### What changes in practice

Each case below assumes a `.loreignore` containing `*.log`. "Before" is the behavior on 0.12.1 or with revision 2044; "after" is this decision.

**1. A force-staged file is visible and can be taken back.**

```
$ lore stage --force debug.log
$ lore status
```

- Before 0.12.1: `debug.log` is not listed. `lore unstage .` leaves it staged, and `lore commit` takes it.
- Revision 2044: listed as staged, and `unstage` reaches it.
- After: the same as 2044, and a file force-staged with unchanged content is also listed, as a staged modification.

**2. A committed ignored file is edited.**

```
$ echo changed > debug.log      # debug.log is in the current revision
$ lore status --scan
$ lore stage --scan
```

- Before: `status --scan` is silent and `stage --scan` stages nothing. The change takes `lore dirty --force debug.log` and then `lore stage --force`.
- After: `debug.log` is reported as modified, and `lore stage --scan` stages it, without `--force`.

**3. An ignore rule is added after the file was committed.**

```
$ lore commit -m "add data.bin"
$ echo "*.bin" >> .loreignore
$ echo changed > data.bin
$ lore status --scan
```

- Before: `data.bin` drops out of `status` and `stage`, though it is still in the revision.
- After: `data.bin` still shows as modified. Like Git, the rule does not apply to a tracked file. To drop it, delete it from the revision.

**4. A new file the rule names is still ignored.**

```
$ echo noise > new.log          # never committed or staged
$ lore status --scan            # new.log is not listed
$ lore stage --scan             # new.log is not staged
```

- Before and after: unchanged. The rule governs what the tree does not hold. `new.log` beside a tracked `debug.log` is ignored while `debug.log` is not.

**5. An untracked file below a tracked, ignored directory.**

```
build/            # ignored by the rule "build/", but build/keep.o is tracked
build/keep.o      # tracked: edited changes are reported
build/new.o       # untracked: stays ignored
```

- Before: neither file is reported.
- After: `build/keep.o` is reported, and `build/new.o` is not, because the ignore slot is still stepped for a tracked directory.

**6. `lore dirty` on a file that was only marked.**

```
$ lore dirty new.log --force    # marks an add; not staged yet
$ lore status                   # lists new.log
```

An add that is only marked dirty is not tracked. After a rule is added that names it, the mark is dropped again, as Git drops an untracked file a new `.gitignore` rule names. Staging it first makes it tracked.

**7. A layer.**

A layer mounted at `lay/` with a tracked `lay/debug.log` and the parent's `*.log` rule: `lore dirty lay/debug.log` marks it without `--force`, while an untracked `lay/new.log` beside it stays ignored.

**8. A committed ignored file is deleted from the working tree.**

```
$ rm debug.log                  # debug.log is in the current revision
$ lore status --scan
$ lore stage --scan
```

- With revision 2066: an unforced `stage` keeps `debug.log` in the revision, and the deletion takes `lore --force stage`. The same holds when the directory holding it is removed, and for a committed empty directory a rule names.
- After: `debug.log` is reported deleted, and `lore stage --scan` stages the deletion, without `--force`. A file the view excludes is still never deleted this way, since a sparse working tree lacks it on purpose.

**9. A directory replaces a committed file.**

```
src/Intermediate      # a committed file
src/Intermediate/     # replaced on disk by a directory, under the rule "Intermediate/"
```

- After: `lore status` reports the file deleted, and leaves out what the directory holds. The tree tracks a file at that path, not a directory, so the directory is new content and the rule names it.

### Consequences

- Good, because staged content is always visible to `status` and reachable by `unstage`, as revision 2044 made it.
- Good, because a committed file keeps flowing through `status --scan`, `stage` and `dirty` whatever the ignore file says, as §12.3 already promised.
- Good, because the behavior matches Git, which is what most users expect.
- Bad, because adding an ignore rule no longer hides changes to files that are already tracked. A user who wants such a file gone from the revision has to delete it from the revision, as in Git.
- Bad, because removing a directory from the working tree now stages the deletion of the committed files below it that the ignore file names, where revision 2066 kept them unless the stage was forced. Git's `git add -A` does the same.
- Bad, because the walks over the working tree look an entry up in the tree before filtering it, where they used to skip an ignored entry unread. The walks that already claim entries against a directory listing pay nothing. A path-level check pays one lookup, and only for a path the filter would otherwise drop.
- Neutral, because the comparison against the staged state now enters an ignored directory that holds dirty marks. It enters only the directories along those marks, which is what it pays for marks anywhere else, and none where nothing is marked.
- Neutral, because a file force-staged with unchanged content now shows in `status` as a staged modification, where it used to be hidden. A commit of nothing else still fails with "Nothing staged for commit", as before, and `unstage` clears it.

## Pros and Cons of the Options

### Ask the ignore file only about paths the tree does not hold

- Good, because nothing in the tree is hidden from any operation that works on it.
- Good, because it is what §12.3, the glossary and Git all describe.
- Bad, because it touches every working-tree walk.

### Keep asking it about every path except what is staged

- Good, because it is already on main, as revision 2044.
- Bad, because a committed ignored file still needs `--force` to stage a change, which is the confusing half of the report.
- Bad, because §12.3 and the glossary would have to be changed to describe it.

### Keep the behavior and document it

- Good, because nothing changes.
- Bad, because a commit can take staged content that status does not show.

## More Information

- `system-design.md` §12.4 is updated with this rule.
- Revision 2044 (change request `01a18d5f-d21e-7130-a073-3bb3ef029e48`) is the base this builds on. Its tests in `lore-revision/tests/dirty.rs` that expected a dirty change below an ignored directory to stay hidden now expect it listed, as does the outbound half of `test_urcignore_committed_file` (`scripts/test/test_urcignore.py`), which expected a committed file to drop out of `status` and `stage` once an ignore rule named it. `test_layer_stage_is_filtered_by_the_mount_path` (`scripts/test/test_layer.py`) keeps its point, that a rule is matched against the mount path, with a new file below the mount instead of a tracked one.
- Revision 2066 (change request `01a18ee4-033f-7590-8491-5bd1376e30d9`) kept tracked content the ignore file names out of deletions. Its tests in `lore-revision/tests/stage_intent.rs` that expected that now expect the deletion: `a_staged_delete_deletes_a_tracked_child_the_ignore_file_names`, `a_staged_delete_deletes_a_directory_holding_only_tracked_content_the_ignore_file_names`, `a_marking_delete_marks_a_tracked_child_the_ignore_file_names` and `a_scanning_stage_deletes_a_tracked_empty_directory_the_ignore_file_names`, and in `lore-revision/tests/stage.rs` `a_stage_scan_stages_the_removal_of_a_tracked_file_the_ignore_file_excludes`, which expected the removal only under `--force`. Its view half is unchanged, and is covered in `lore-revision/tests/sync_views.rs`.
- A directory a forced dirty mark creates is covered by `a_forced_dirty_add_below_a_new_ignored_directory_stays_ignored` (`lore-revision/tests/dirty.rs`) and `a_new_directory_is_recorded_only_where_it_is_empty` (`lore-revision/tests/unit/state.rs`).
- What a dirty mark leaves untracked is covered by `a_dirty_copy_into_an_ignored_directory_stays_ignored_and_a_dirty_move_does_not` (`lore-revision/tests/dirty.rs`) and `a_tracked_path_the_ignore_file_names_is_recorded_and_a_marked_add_or_copy_is_not` (`lore-revision/tests/unit/state.rs`), which build the tree on a committed revision.
- The replay of dirty marks is covered by `rebase_staged_state_carries_a_dirty_tracked_file_an_ignore_rule_names` and `a_commit_carries_a_dirty_tracked_file_an_ignore_rule_names` (`lore-revision/tests/dirty.rs`).
- The rule as the kind the tree holds is covered by revision 2055's `a_status_of_a_file_replaced_by_a_directory_a_directory_only_rule_names_reports_its_removal` (`lore-revision/tests/dirty.rs`).
- The lock commands are covered by `a_lock_on_a_tracked_file_the_ignore_file_names_is_asked_for` and `a_lock_on_a_staged_file_the_ignore_file_names_is_asked_for` (`lore-revision/tests/unit/lock/file/acquire.rs`) and `scripts/test/regression/test_lock_tracked_ignored_file.py`, and `file info` by `a_tracked_file_the_ignore_file_names_is_measured`, `a_tracked_directory_the_ignore_file_names_measures_what_the_tree_tracks`, `a_directory_measures_a_tracked_file_the_ignore_file_names_below_it`, `a_file_and_a_directory_of_one_folded_name_are_each_measured_as_their_kind`, `a_staged_file_the_ignore_file_names_is_measured`, `a_file_a_dirty_move_took_into_an_ignored_directory_is_measured`, `an_untracked_file_the_ignore_file_names_is_left_out` and `a_revision_decides_what_file_info_tracks` (`lore-revision/tests/unit/file/info.rs`), and below a link mount by `scripts/test/link/test_link_file_info.py`.
- A forced stage of a directory is covered by the tests in `lore-revision/tests/unit/file/stage/force_tests.rs`.
- **Amended 2026-11-11:** The view kept in the comparison is covered by `a_staged_node_passes_the_view_slot_only_in_a_comparison` (`lore-revision/tests/unit/filter.rs`), `status_lists_a_forced_stage_scan_past_the_view`, `only_a_forced_unstage_takes_back_a_forced_stage_scan_past_the_view` and, for the walk below a directory a stage added, `status_leaves_out_a_file_dirty_moved_below_a_directory_added_past_the_view` (`lore-revision/tests/dirty.rs`), `test_forced_stage_scan_commits_files_the_view_excludes` and `test_forced_stage_scan_commits_files_the_view_excludes_below_a_link_and_a_layer` (`scripts/test/test_view.py`) and, for a merge, `test_merge_out_of_view_conflict_takes_theirs` (`scripts/test/test_merge_view.py`).
- The rule is covered by `a_tracked_step_answers_to_the_view_alone` (`lore-revision/tests/filter_gitignore.rs`), `a_tracked_node_passes_the_ignore_slot_and_its_untracked_children_do_not` (`lore-revision/tests/unit/filter.rs`), `status_lists_forced_and_dirty_changes_below_an_ignored_directory` and `the_staged_comparison_enters_an_ignored_directory_only_along_its_changes` (`lore-revision/tests/dirty.rs`), `a_marking_walk_reports_a_tracked_file_the_ignore_file_names` (`lore-revision/tests/stage_intent.rs`) and the regression tests in `scripts/test/regression/test_force_staged_ignored_file.py`, which cover a layer too.
