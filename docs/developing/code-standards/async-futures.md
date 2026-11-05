# Lore async future standards

This document defines how to write futures, boxes, and spawned tasks so that future sizes, poll frames, and heap allocations stay bounded.

## Overview

A future is stored as an enum of its suspension states. Its size is its largest state plus a prefix reserved in every state.

| Held in the prefix | Cause |
| --- | --- |
| The arguments of an `async fn` | Captured for the future's whole life. An argument used after an await is stored a second time, in the local it is moved into. |
| The captures of an `async move` block | Captured for the future's whole life, once. |
| A local live across two or more awaits | Reserved in every state, including states before it exists. |

| Held in a state after its last use | Cause |
| --- | --- |
| A value a pattern moves part of | `while let`, `for`, `if let`, `match`, and `select!` arms keep the scrutinee until its scope ends. |
| A borrowed local, or a local of a `Copy` type | Kept until its scope ends. |
| A shadowed local | Shadowing does not end storage. |
| The futures of `join!` | Held side by side: the sum, not the maximum. |

A future awaited inline is part of its parent's state, so a byte a callee holds is held by every future above it.

## 1. Allocation

- Don't add a heap allocation to reduce a future or a frame on a path that runs per item: per fragment, file, node, request, or connection. Reduce what the future holds. Once per client command or C API call isn't per item.
- Box only a cold path: a cache miss, an error or recovery path, a rarely taken branch. State in the function's doc comment that the path is cold.
- Remove a box whose future fits in its parent's largest state. The box costs an allocation and saves nothing.
- Use `async fn`, or `fn` returning `impl Future`, in a trait that is never used as `dyn`. `#[async_trait]` boxes every call.
- Build large shared data in its allocation with `Arc::new_uninit` and a write per field, as `State::new_shared` does. `Arc::new(value)` can build `value` on the stack and copy it.

```rust
/// Only the load is boxed, as in [`Self::block`]. Inline, it would make every future awaiting
/// a lookup as large as the load's, whether or not that lookup loads.
pub async fn block_file_metadata(
    &self,
    repository: Arc<RepositoryContext>,
    block_index: usize,
) -> Result<Arc<NodeFileMetadataBlock>, StateError> {
    {
        let lock = self.runtime.read();
        if lock.block_file_metadata.len() > block_index
            && let Some(block) = lock.block_file_metadata[block_index].upgrade()
        {
            return Ok(block);
        }
    }

    Box::pin(self.block_file_metadata_load(repository, block_index)).await
}
```

## 2. Arguments

- Write a function whose future is paid per item or sets the size of a command's future, and that keeps an argument larger than a pointer across an await, as a `fn` returning an `async move` block that uses its captures in place. Rebinding a capture stores it twice. Add `#[allow(clippy::manual_async_fn)]` and the doc line "Not an `async fn`, which would hold a second copy of its arguments."
- Return the inner future from a function that only forwards, rather than awaiting it.
- Pass data the caller keeps by reference.
- Take a value apart before building the future when the future uses only some of its fields.
- Capture every guard argument of a hand-written future in its block. An uncaptured argument is dropped when the function returns, not when the future completes.

```rust
/// Not an `async fn`, which would hold a second copy of its arguments.
#[allow(clippy::manual_async_fn)]
fn emit_diff_item_with_auto_resolve(
    mut item: DiffItem,
    auto_resolve: bool,
    tx: &mpsc::Sender<Result<DiffItem, BranchError>>,
) -> impl Future<Output = Result<(), BranchError>> + '_ {
    async move {
        if auto_resolve
            && let DiffItem::Conflict(pair) = &item
            && let Some(resolved) = Box::pin(try_auto_resolve_conflict(&pair.0, &pair.1)).await?
        {
            item = DiffItem::Change(resolved);
        }
        let permit = tx
            .reserve()
            .await
            .map_err(|_closed| Internal::msg("diff3 channel closed"))?;
        permit.send(Ok(item));
        Ok(())
    }
}
```

## 3. Locals across awaits

- End a local's scope before the next await when it is not used after it.
- Keep the fields read from a large `Copy` value, not a copy of the value.
- Move a phase whose locals cross two of its awaits into a function of its own, so its locals are not reserved during the other phases.

```rust
// Holds the 32-byte hash across the read, not the 288-byte `Tree`.
let hash_node = state.tree(repository.clone()).await?.hash_node;
```

## 4. Loops, matches, and `select!`

- Take items with `let`-`else` in a `loop` when the body awaits. A `while let` scrutinee stays reserved beside the binding.
- Iterate by reference when the body awaits.
- Reduce an item to what the loop needs before the next await.
- Return the received value from a `select!` and await after it. Await a producer that outlives its channel outside the `select!`.
- Match an awaited `Result` directly, not a named local that `if let Ok(value) = local` moves out of.

```rust
// Holds the scrutinee and `change` across the report.
while let Some(change) = changes.next().await {
    report_scan_change(operation, repository, summary, &change).await?;
}

// Holds `change` alone.
loop {
    let Some(change) = changes.next().await else {
        break;
    };
    report_scan_change(operation, repository, summary, &change).await?;
}
```

## 5. Pinning, dispatch, and stack frames

- Do not pin a future captured by an `async move` block or passed to an `async fn`. `pin!` moves it into a local, so it is held twice. Pin it in the caller and pass `Pin<&mut impl Future>`, or build it inside the block.
- Dispatch over many variants by pinning each variant's future in a non-inlined frame of its own and awaiting it as `Pin<&mut dyn Future>`. An enum future is as large as its largest variant.
- Take a large value by reference. Passing it by value copies it onto the stack.
- Mark `#[inline]` a plain `fn` that does work before returning a large future. Called from another codegen unit without inlining, it makes its caller build the future in a stack temporary and copy it.

```rust
#[inline(never)]
pub(crate) fn run_handler<A: InvokableLoreArgs>(
    command: LoreCommand,
    globals: LoreGlobalArgs,
    callback: LoreEventCallback,
    take_args: impl FnOnce(LoreCommand) -> A,
) -> i32 {
    block_on_command(pin!(take_args(command).invoke_local(globals, callback)))
}
```

## 6. Channels

- Reserve the slot, then build the item in the permit's `send` call. The future of `send(item)` holds the item while it waits.

```rust
let Ok(permit) = clone.file_tx.reserve().await else {
    return Err(CloneError::internal("Recursion task failed"));
};
permit.send(CloneWorkItem {
    repository: dispatcher.repository.clone(),
    node,
    repository_path: node_path,
});
```

## 7. Spawning

- Spawn the callee's future itself, not a block that awaits it. The block holds the moved arguments beside the callee's future for the task's life. Move permits and counters into the callee.
- Poll a future that needs bookkeeping through a pin-projected wrapper. A block that captures and awaits it holds it twice.
- Spawn per bounded unit of work. Each spawn allocates a task as large as its future.

```rust
lore_spawn!(tasks, count_worker(shared));
```

## 8. Recursion

- Walk a tree with an explicit worklist and await the per-directory future inline in the loop. A recursive `Box::pin` allocates per level, and stack use grows with depth.

## 9. Cancellation

- Assume a future is dropped at any await. Give work on another thread only memory that work owns, or memory whose release waits for that work. A buffer made by extending a borrow's lifetime, such as `Bytes::from_static` over a borrowed slice, is neither.

## 10. Measurement and tests

- Measure every change to async code against its base with `scripts/type-sizes.py`, and report the changed sizes in the commit message. Build the base and the change in the same working tree. `build` recompiles every workspace crate in release and records each future's states, `compare` lists the futures whose size changed, and `show` prints what each state of a future holds.

  ```sh
  python3 scripts/type-sizes.py build base     # on the base revision
  python3 scripts/type-sizes.py build change   # on the change
  python3 scripts/type-sizes.py compare base change
  python3 scripts/type-sizes.py show change '<regex>'
  ```

- For a change that adds or removes a box, count the allocations of each affected command against the base with `scripts/allocations/compare.py`, and report them in the commit message. The script runs on Linux. Each `build` leaves its binary in `target/type-sizes/<label>-lore`. `--cwd` names the repository to run in, and `--setup` restores it before each run of a command that changes it.

  ```sh
  python3 scripts/allocations/compare.py target/type-sizes/base-lore target/type-sizes/change-lore \
      --cwd <repository> -- <lore arguments>
  ```

- Add a size test only where the smaller layout depends on a structure a later edit could undo unnoticed: a hand-written future, a `let`-`else` in place of a `while let`, a future pinned by its caller, a boxed cold path.
- Make a size test relative: construct the future without polling and compare `size_of_val` with the futures and types it must not hold. Do not use a fixed byte budget. Layouts differ per platform.
- Confirm that a size test fails without the change.
- Treat clippy's `large_futures` threshold as a backstop. Satisfy it by boxing a cold path, never a hot one.
- Treat debug builds as bounded only by not overflowing the stack.

```rust
assert!(
    size_of_val(&step) < 2 * size_of::<DiffItem>(),
    "the step holds {} bytes for an item of {}",
    size_of_val(&step),
    size_of::<DiffItem>()
);
```

## See also

- [Task spawning](tasks.md): the `lore_spawn!` macros and `JoinSet`.
- [Engineering principles](engineering-principles.md): performance and determinism.
