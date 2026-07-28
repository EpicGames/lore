# Lore commenting and documentation standards

This document defines the standard patterns for code documentation across the Lore codebase.

## Function documentation

Every crate-public function must have a short rustdoc comment at minimum.

If the function is complex enough to warrant a code example it should be in correct code and pass the Rust
doc test, not ignored.

## Code comments best practices

These rules apply to every comment, wherever it appears.

- Prefer self-describing code. Encode invariants in types so the compiler checks them wherever it can.
- When the compiler cannot capture a semantic or an invariant, express it through the function name and the
  argument names first. Use a comment only for behaviour or semantics that naming cannot convey.
- Write each comment about the thing it documents. Do not describe specific consumers or use cases.
- Comments shouldn't be used to group functions into sections.
- Comments shouldn't be used for code that is self-explanatory from the code itself.
- Comments can be used to document complex logic and dependencies between different code sections.

Keep comments as short as possible. Use clear language in the active voice. Avoid metaphors and slang.
Keep the language suitable for non-native English speakers.

## The interface crate

The `extern "C"` functions in `lore/src/interface.rs` form the public C API. The build script
(`lore/build.rs`) runs cbindgen to copy each function's rustdoc comment into the generated C header
`lore-capi/lore.h`, where it becomes a C comment. The comment you write in Rust is the comment a C
consumer reads.

Write these comments for the C consumer:

- Refer to the C names and types that appear in `lore.h`, such as `lore_event_t`, `LORE_EVENT_LOG`, and
  `lore_auth_user_info`. Do not refer to the Rust type paths behind them.
- Describe the C API contract: return values, events delivered to the callback, and the lifetime of any
  pointer or string the caller passes in or receives.

The rules in the previous section still apply. Keep each comment short, in the active voice, and readable
for non-native English speakers.
