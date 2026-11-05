---
name: review-change
description: Must be used before submitting any change to Lore, and to review another author's change. Reviews the change adversarially, in a fresh session, against code-review.md and the standards it names. On your own branch it fixes the findings that have one obvious fix and reports the rest; on another author's change it reports the findings by severity.
argument-hint: [branch; default the current branch]
---

# Review change

Review a change to Lore as `docs/developing/code-standards/code-review.md` describes. The rules live in that file and in the standards its Guidance names. This skill is the procedure for applying them, and restates only what a step needs.

## Arguments

- **No arguments**: review the current branch against its parent branch.
- **`<branch>`**: review that branch, another author's change.

Findings from an earlier review, and comments already on the review, are input to verify, not conclusions.

## Step 1: Establish the change

Find the branch, its base, and the change's description. For your own branch, `lore branch info` names the parent branch, `lore branch diff <parent>` lists the committed changes, and `lore status` lists the changes not yet committed. Review both.

Check out another author's branch in an instance of its own over the shared store, with `lore clone <remote-url> <path> --branch <branch> --use-shared-store`. Don't switch your own working tree to it. In that instance, `lore branch diff <parent>` lists the changes and names the base revision they're computed from. Compare files against that base, checked out with `lore clone <remote-url> <path> --revision <base> --use-shared-store`, not against the parent's latest revision, which also holds the parent's newer changes.

In a git clone, `git merge-base`, `git diff --name-only <base>` and `git status` give the same lists, and `git worktree add` checks out another author's branch.

Record the revision you review, and whether the working tree holds changes beyond it. Every finding refers to it.

## Step 2: Review in a fresh session

You must spawn a subagent to review: the context that wrote the change carries its reasoning, and a review from it confirms what the author meant rather than what the code does. Give it the instance path, the branch, its base, the revision, the change's description, and the earlier findings and comments. Don't give it the reasoning behind the change.

Tell the subagent to:

1. Load the `lore-development` skill, and skip its review step: the subagent is that review.
2. Read `code-review.md` and every standard its Guidance names, in full and in its own session. Of `system-design.md` and the ADRs, read the sections and decisions for the area the change touches.
3. Read each changed function and type in full with its doc comments, each changed doc page in full, and the code the change calls and is called from. Read from the code, not from memory.
4. Review the change as `code-review.md` describes. Performance and allocations come first.
5. Return each finding as *Reporting findings* in `code-review.md` describes, and edit no file.
6. List the standards it read at the top of its findings.

A change that adds or edits a proposal in `docs/proposals/` is also reviewed with the `review-lep` skill, in a second subagent given the same input, spawned beside the first. Report its findings in its own Required, Recommended and Optional buckets, after the findings by severity.

## Step 3: Verify the findings

Check each finding against the code yourself. Drop a finding the code doesn't support, and say why. Don't add a finding you haven't checked.

## Step 4: Match tracked work

When `context/team-workspace.md` exists, search the work items it describes for open and deferred items that cover each finding. Refer to a covered finding by its work item rather than fixing or filing it again. A finding a work item defers stays deferred unless the user decides otherwise. Without that file, or without access to the work items, skip this step and say so.

## Step 5: Act on the findings

### Your own branch

1. Fix each finding that has one obvious fix. A defect's fix must start with a test that fails without it, as `testing.md` describes in *What a change tests*.
2. Build and run the tests the fixes touch, as the `lore-development` skill describes.
3. Record the state of the working tree again, and review again from step 2 in a new subagent, with the fixed findings as input. Repeat until a review reports nothing new, for at most three rounds, then report what remains.
4. Report the remaining findings to the user by severity: those with several reasonable fixes, design choices and trade-offs. Give the options for each, and recommend one.
5. File each finding left for later as a work item, where step 4 found work items to search.

### Another author's change

1. Report the findings in the conversation by severity. Don't change the author's code.
2. Post the findings to the review only when the user asks. Post one comment per severity, and name the revision reviewed at the top of each.
3. Refer to a point an existing comment already makes rather than repeating it.
