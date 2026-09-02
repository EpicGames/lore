use std::{ops::Range, path::is_separator};

#[derive(Clone, Copy, Debug, Default)]
struct State {
  // These store character indices into the glob and path strings.
  path_index: usize,
  glob_index: usize,

  // The current index into the captures list.
  capture_index: usize,

  // When we hit a * or **, we store the state for backtracking.
  wildcard: Wildcard,
  globstar: Wildcard,
}

#[derive(Clone, Copy, Debug, Default)]
struct Wildcard {
  // Using u32 rather than usize for these results in 10% faster performance.
  glob_index: u32,
  path_index: u32,
  capture_index: u32,
}

type Capture = Range<usize>;

/// Whether `glob` matches `path`.
///
/// LORE: `glob_match_internal` can miss a match where a `**` is followed by a
/// component holding a wildcard, because it commits the `**` backtrack point on
/// the first literal match and cannot restore it. It never reports a match that
/// is not there, so confirming its negatives against `doublestar` is sound, and
/// only that shape of glob pays for the second pass. See `tests/doublestar.rs`.
pub fn glob_match(glob: &str, path: &str) -> bool {
  if glob_match_internal(glob, path, None) {
    return true;
  }
  if doublestar::may_be_missed(glob.as_bytes()) && doublestar::is_plain_path(path.as_bytes()) {
    return doublestar::matches(glob.as_bytes(), path.as_bytes());
  }
  false
}

/// LORE: a straightforward, correct matcher used only to confirm negatives from
/// the fast path, for the glob shapes it is known to get wrong.
///
/// Component-wise dynamic programming, so it is polynomial where a naive
/// recursive `**` search is exponential: `**` may absorb any number of
/// components, and each `(glob component, path component)` pair is decided once.
mod doublestar {
  /// Whether `glob` can trip the fast path's `**` defect: a `**` component with a
  /// later component holding a wildcard.
  ///
  /// Also refuses the constructs this matcher does not implement -- brace
  /// alternation, leading negation and escapes -- so a glob using them is never
  /// answered from here. Those cannot combine with the defect in lore's filters,
  /// which emit neither, and gitignore has no brace syntax at all.
  pub fn may_be_missed(glob: &[u8]) -> bool {
    // Rejected cheaply first, because this runs on every failed match and most
    // globs hold no `**` at all: a byte scan with no splitting or allocation.
    if !glob.windows(2).any(|pair| pair == b"**") {
      return false;
    }
    if glob.first() == Some(&b'!') {
      return false;
    }
    let mut seen_globstar = false;
    for component in glob.split(|&c| c == b'/') {
      if component.iter().any(|c| matches!(c, b'{' | b'}' | b'\\')) {
        return false;
      }
      if seen_globstar && component.iter().any(|c| matches!(c, b'*' | b'?' | b'[')) {
        return true;
      }
      if component == b"**" {
        seen_globstar = true;
      }
    }
    false
  }

  /// Whether `path` has no empty component, i.e. no leading, trailing or
  /// doubled separator.
  ///
  /// The fast path treats an empty component as unmatchable, so `a/**/*` does not
  /// match `a/`, while a component-wise matcher would let `*` match the empty
  /// name. Rather than reproduce that corner, the confirming matcher declines
  /// such a path and the fast path's answer stands. Relative paths from the
  /// repository are already normalized, so this never declines a real query.
  pub fn is_plain_path(path: &[u8]) -> bool {
    !path.is_empty()
      && path.first() != Some(&b'/')
      && path.last() != Some(&b'/')
      && !path.windows(2).any(|w| w == b"//")
  }

  /// Whether `glob` matches `path`, both as `/`-separated component lists.
  ///
  /// A `**` in the final position requires at least one remaining component,
  /// which is what gitignore means by "a trailing `/**` matches everything
  /// inside"; elsewhere it may absorb none, so `a/**/b` matches `a/b`. Both
  /// follow the fast path's own behaviour.
  pub fn matches(glob: &[u8], path: &[u8]) -> bool {
    let pattern: Vec<&[u8]> = glob.split(|&c| c == b'/').collect();
    let names: Vec<&[u8]> = path.split(|&c| c == b'/').collect();
    let (m, n) = (pattern.len(), names.len());

    // reachable[j] == "glob components from `i` on can match path components from
    // `j` on", rewritten in place for each `i`. Both directions read a cell before
    // overwriting it, so no second row is needed: the literal case ascends and
    // reads `j + 1`, the `**` case descends and accumulates `j`.
    let mut reachable = vec![false; n + 1];
    reachable[n] = true;
    for i in (0..m).rev() {
      if pattern[i] == b"**" {
        if i + 1 == m {
          // A trailing `**` needs at least one component of its own.
          for j in 0..=n {
            reachable[j] = j < n;
          }
        } else {
          let mut any = false;
          for j in (0..=n).rev() {
            any |= reachable[j];
            reachable[j] = any;
          }
        }
      } else {
        for j in 0..n {
          reachable[j] = reachable[j + 1] && component(pattern[i], names[j]);
        }
        reachable[n] = false;
      }
    }
    reachable[0]
  }

  /// Whether one path component matches one glob component. `*` and `?` do not
  /// cross a separator, and there is none inside a component.
  fn component(pattern: &[u8], name: &[u8]) -> bool {
    // Linear two-pointer star matching: on a mismatch, resume after the last
    // `*` with one more character consumed.
    let (mut p, mut s) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while s < name.len() {
      match pattern.get(p) {
        Some(b'*') => {
          star = Some((p, s));
          p += 1;
        }
        Some(b'?') => {
          p += 1;
          s += 1;
        }
        Some(b'[') => {
          let Some(end) = pattern[p..].iter().position(|&c| c == b']').map(|e| p + e) else {
            return false;
          };
          if class(&pattern[p + 1..end], name[s]) {
            p = end + 1;
            s += 1;
          } else if let Some((sp, ss)) = star {
            p = sp + 1;
            s = ss + 1;
            star = Some((sp, ss + 1));
          } else {
            return false;
          }
        }
        Some(&c) if c == name[s] => {
          p += 1;
          s += 1;
        }
        _ => {
          if let Some((sp, ss)) = star {
            p = sp + 1;
            s = ss + 1;
            star = Some((sp, ss + 1));
          } else {
            return false;
          }
        }
      }
    }
    pattern[p..].iter().all(|&c| c == b'*')
  }

  /// Whether `c` is in the bracket expression `set`, which excludes the
  /// surrounding brackets.
  fn class(set: &[u8], c: u8) -> bool {
    let (negated, set) = match set.first() {
      Some(b'!') | Some(b'^') => (true, &set[1..]),
      _ => (false, set),
    };
    let mut hit = false;
    let mut i = 0;
    while i < set.len() {
      if i + 2 < set.len() && set[i + 1] == b'-' {
        if c >= set[i] && c <= set[i + 2] {
          hit = true;
        }
        i += 3;
      } else {
        if c == set[i] {
          hit = true;
        }
        i += 1;
      }
    }
    hit != negated
  }
}

pub fn glob_match_with_captures<'a>(glob: &str, path: &'a str) -> Option<Vec<Capture>> {
  let mut captures = Vec::new();
  if glob_match_internal(glob, path, Some(&mut captures)) {
    return Some(captures);
  }
  None
}

fn glob_match_internal<'a>(
  glob: &str,
  path: &'a str,
  mut captures: Option<&mut Vec<Capture>>,
) -> bool {
  // This algorithm is based on https://research.swtch.com/glob
  let glob = glob.as_bytes();
  let path = path.as_bytes();

  let mut state = State::default();

  // Store the state when we see an opening '{' brace in a stack.
  // Up to 10 nested braces are supported.
  let mut brace_stack = BraceStack::default();

  // First, check if the pattern is negated with a leading '!' character.
  // Multiple negations can occur.
  let mut negated = false;
  while state.glob_index < glob.len() && glob[state.glob_index] == b'!' {
    negated = !negated;
    state.glob_index += 1;
  }

  while state.glob_index < glob.len() || state.path_index < path.len() {
    if state.glob_index < glob.len() {
      match glob[state.glob_index] {
        b'*' => {
          let is_globstar = state.glob_index + 1 < glob.len() && glob[state.glob_index + 1] == b'*';
          if is_globstar {
            // Coalesce multiple ** segments into one.
            state.glob_index = skip_globstars(glob, state.glob_index + 2) - 2;
          }

          // If we are on a different glob index than before, start a new capture.
          // Otherwise, extend the active one.
          if captures.is_some()
            && (captures.as_ref().unwrap().is_empty()
              || state.glob_index != state.wildcard.glob_index as usize)
          {
            state.wildcard.capture_index = state.capture_index as u32;
            state.begin_capture(&mut captures, state.path_index..state.path_index);
          } else {
            state.extend_capture(&mut captures);
          }

          state.wildcard.glob_index = state.glob_index as u32;
          state.wildcard.path_index = state.path_index as u32 + 1;

          // ** allows path separators, whereas * does not.
          // However, ** must be a full path component, i.e. a/**/b not a**b.
          if is_globstar {
            state.glob_index += 2;

            if glob.len() == state.glob_index {
              // A trailing ** segment without a following separator.
              state.globstar = state.wildcard;
            } else if (state.glob_index < 3 || glob[state.glob_index - 3] == b'/')
              && glob[state.glob_index] == b'/'
            {
              // Matched a full /**/ segment. If the last character in the path was a separator,
              // skip the separator in the glob so we search for the next character.
              // In effect, this makes the whole segment optional so that a/**/b matches a/b.
              if state.path_index == 0
                || (state.path_index < path.len()
                  && is_separator(path[state.path_index - 1] as char))
              {
                state.end_capture(&mut captures);
                state.glob_index += 1;
              }

              // The allows_sep flag allows separator characters in ** matches.
              // one is a '/', which prevents a/**/b from matching a/bb.
              state.globstar = state.wildcard;
            }
          } else {
            state.glob_index += 1;
          }

          // If we are in a * segment and hit a separator,
          // either jump back to a previous ** or end the wildcard.
          if state.globstar.path_index != state.wildcard.path_index
            && state.path_index < path.len()
            && is_separator(path[state.path_index] as char)
          {
            // Special case: don't jump back for a / at the end of the glob.
            if state.globstar.path_index > 0 && state.path_index + 1 < path.len() {
              state.glob_index = state.globstar.glob_index as usize;
              state.capture_index = state.globstar.capture_index as usize;
              state.wildcard.glob_index = state.globstar.glob_index;
              state.wildcard.capture_index = state.globstar.capture_index;
            } else {
              state.wildcard.path_index = 0;
            }
          }

          // If the next char is a special brace separator,
          // skip to the end of the braces so we don't try to match it.
          if brace_stack.length > 0
            && state.glob_index < glob.len()
            && matches!(glob[state.glob_index], b',' | b'}')
          {
            if state.skip_braces(glob, &mut captures, false) == BraceState::Invalid {
              // invalid pattern!
              return false;
            }
          }

          continue;
        }
        b'?' if state.path_index < path.len() => {
          if !is_separator(path[state.path_index] as char) {
            state.add_char_capture(&mut captures);
            state.glob_index += 1;
            state.path_index += 1;
            continue;
          }
        }
        b'[' if state.path_index < path.len() => {
          state.glob_index += 1;
          let c = path[state.path_index];

          // Check if the character class is negated.
          let mut negated = false;
          if state.glob_index < glob.len() && matches!(glob[state.glob_index], b'^' | b'!') {
            negated = true;
            state.glob_index += 1;
          }

          // Try each range.
          let mut first = true;
          let mut is_match = false;
          while state.glob_index < glob.len() && (first || glob[state.glob_index] != b']') {
            let mut low = glob[state.glob_index];
            if !unescape(&mut low, glob, &mut state.glob_index) {
              // Invalid pattern!
              return false;
            }
            state.glob_index += 1;

            // If there is a - and the following character is not ], read the range end character.
            let high = if state.glob_index + 1 < glob.len()
              && glob[state.glob_index] == b'-'
              && glob[state.glob_index + 1] != b']'
            {
              state.glob_index += 1;
              let mut high = glob[state.glob_index];
              if !unescape(&mut high, glob, &mut state.glob_index) {
                // Invalid pattern!
                return false;
              }
              state.glob_index += 1;
              high
            } else {
              low
            };

            if low <= c && c <= high {
              is_match = true;
            }
            first = false;
          }
          if state.glob_index >= glob.len() {
            // invalid pattern!
            return false;
          }
          state.glob_index += 1;
          if is_match != negated {
            state.add_char_capture(&mut captures);
            state.path_index += 1;
            continue;
          }
        }
        b'{' if state.path_index < path.len() => {
          if brace_stack.length as usize >= brace_stack.stack.len() {
            // Invalid pattern! Too many nested braces.
            return false;
          }

          state.end_capture(&mut captures);
          state.begin_capture(&mut captures, state.path_index..state.path_index);

          // Push old state to the stack, and reset current state.
          state = brace_stack.push(&state);
          continue;
        }
        b'}' if brace_stack.length > 0 => {
          // If we hit the end of the braces, we matched the last option.
          brace_stack.longest_brace_match =
            brace_stack.longest_brace_match.max(state.path_index as u32);
          state.glob_index += 1;
          state = brace_stack.pop(&state, &mut captures);
          continue;
        }
        b',' if brace_stack.length > 0 => {
          // If we hit a comma, we matched one of the options!
          // But we still need to check the others in case there is a longer match.
          brace_stack.longest_brace_match =
            brace_stack.longest_brace_match.max(state.path_index as u32);
          state.path_index = brace_stack.last().path_index;
          state.glob_index += 1;
          state.wildcard = Wildcard::default();
          state.globstar = Wildcard::default();
          continue;
        }
        mut c if state.path_index < path.len() => {
          // Match escaped characters as literals.
          if !unescape(&mut c, glob, &mut state.glob_index) {
            // Invalid pattern!
            return false;
          }

          let is_match = if c == b'/' {
            is_separator(path[state.path_index] as char)
          } else {
            path[state.path_index] == c
          };

          if is_match {
            state.end_capture(&mut captures);

            if brace_stack.length > 0 && state.glob_index > 0 && glob[state.glob_index - 1] == b'}'
            {
              brace_stack.longest_brace_match = state.path_index as u32;
              state = brace_stack.pop(&state, &mut captures);
            }
            state.glob_index += 1;
            state.path_index += 1;

            // If this is not a separator, lock in the previous globstar.
            //
            // LORE: this commit is irreversible, and that is the `**`
            // backtracking defect -- see `doublestar` at the bottom of this
            // file. Upstream's state machine is left exactly as it was; the
            // correction happens in `glob_match`.
            if c != b'/' {
              state.globstar.path_index = 0;
            }
            continue;
          }
        }
        _ => {}
      }
    }

    // If we didn't match, restore state to the previous star pattern.
    if state.wildcard.path_index > 0 && state.wildcard.path_index as usize <= path.len() {
      state.backtrack();
      continue;
    }

    if brace_stack.length > 0 {
      // If in braces, find next option and reset path to index where we saw the '{'
      match state.skip_braces(glob, &mut captures, true) {
        BraceState::Invalid => return false,
        BraceState::Comma => {
          state.path_index = brace_stack.last().path_index;
          continue;
        }
        BraceState::EndBrace => {}
      }

      // Hit the end. Pop the stack.
      // If we matched a previous option, use that.
      if brace_stack.longest_brace_match > 0 {
        state = brace_stack.pop(&state, &mut captures);
        continue;
      } else {
        // Didn't match. Restore state, and check if we need to jump back to a star pattern.
        state = *brace_stack.last();
        brace_stack.length -= 1;
        if let Some(captures) = &mut captures {
          captures.truncate(state.capture_index);
        }
        if state.wildcard.path_index > 0 && state.wildcard.path_index as usize <= path.len() {
          state.backtrack();
          continue;
        }
      }
    }

    return negated;
  }

  if brace_stack.length > 0 && state.glob_index > 0 && glob[state.glob_index - 1] == b'}' {
    brace_stack.longest_brace_match = state.path_index as u32;
    brace_stack.pop(&state, &mut captures);
  }

  !negated
}

#[inline(always)]
fn unescape(c: &mut u8, glob: &[u8], glob_index: &mut usize) -> bool {
  if *c == b'\\' {
    *glob_index += 1;
    if *glob_index >= glob.len() {
      // Invalid pattern!
      return false;
    }
    *c = match glob[*glob_index] {
      b'a' => b'\x61',
      b'b' => b'\x08',
      b'n' => b'\n',
      b'r' => b'\r',
      b't' => b'\t',
      c => c,
    }
  }
  true
}

#[derive(PartialEq)]
enum BraceState {
  Invalid,
  Comma,
  EndBrace,
}

impl State {
  #[inline(always)]
  fn backtrack(&mut self) {
    self.glob_index = self.wildcard.glob_index as usize;
    self.path_index = self.wildcard.path_index as usize;
    self.capture_index = self.wildcard.capture_index as usize;
  }

  #[inline(always)]
  fn begin_capture(&self, captures: &mut Option<&mut Vec<Capture>>, capture: Capture) {
    if let Some(captures) = captures {
      if self.capture_index < captures.len() {
        captures[self.capture_index] = capture;
      } else {
        captures.push(capture);
      }
    }
  }

  #[inline(always)]
  fn extend_capture(&self, captures: &mut Option<&mut Vec<Capture>>) {
    if let Some(captures) = captures {
      if self.capture_index < captures.len() {
        captures[self.capture_index].end = self.path_index;
      }
    }
  }

  #[inline(always)]
  fn end_capture(&mut self, captures: &mut Option<&mut Vec<Capture>>) {
    if let Some(captures) = captures {
      if self.capture_index < captures.len() {
        self.capture_index += 1;
      }
    }
  }

  #[inline(always)]
  fn add_char_capture(&mut self, captures: &mut Option<&mut Vec<Capture>>) {
    self.end_capture(captures);
    self.begin_capture(captures, self.path_index..self.path_index + 1);
    self.capture_index += 1;
  }

  fn skip_braces(
    &mut self,
    glob: &[u8],
    captures: &mut Option<&mut Vec<Capture>>,
    stop_on_comma: bool,
  ) -> BraceState {
    let mut braces = 1;
    let mut in_brackets = false;
    let mut capture_index = self.capture_index + 1;
    while self.glob_index < glob.len() && braces > 0 {
      match glob[self.glob_index] {
        // Skip nested braces.
        b'{' if !in_brackets => braces += 1,
        b'}' if !in_brackets => braces -= 1,
        b',' if stop_on_comma && braces == 1 && !in_brackets => {
          self.glob_index += 1;
          return BraceState::Comma;
        }
        c @ (b'*' | b'?' | b'[') if !in_brackets => {
          if c == b'[' {
            in_brackets = true;
          }
          if let Some(captures) = captures {
            if capture_index < captures.len() {
              captures[capture_index] = self.path_index..self.path_index;
            } else {
              captures.push(self.path_index..self.path_index);
            }
            capture_index += 1;
          }
          if c == b'*' {
            if self.glob_index + 1 < glob.len() && glob[self.glob_index + 1] == b'*' {
              self.glob_index = skip_globstars(glob, self.glob_index + 2) - 2;
              self.glob_index += 1;
            }
          }
        }
        b']' => in_brackets = false,
        b'\\' => {
          self.glob_index += 1;
        }
        _ => {}
      }
      self.glob_index += 1;
    }

    if braces != 0 {
      return BraceState::Invalid;
    }

    BraceState::EndBrace
  }
}

#[inline(always)]
fn skip_globstars(glob: &[u8], mut glob_index: usize) -> usize {
  // Coalesce multiple ** segments into one.
  while glob_index + 3 <= glob.len()
    && unsafe { glob.get_unchecked(glob_index..glob_index + 3) } == b"/**"
  {
    glob_index += 3;
  }
  glob_index
}

struct BraceStack {
  stack: [State; 10],
  length: u32,
  longest_brace_match: u32,
}

impl Default for BraceStack {
  #[inline]
  fn default() -> Self {
    // Manual implementation is faster than the automatically derived one.
    BraceStack {
      stack: [State::default(); 10],
      length: 0,
      longest_brace_match: 0,
    }
  }
}

impl BraceStack {
  #[inline(always)]
  fn push(&mut self, state: &State) -> State {
    // Push old state to the stack, and reset current state.
    self.stack[self.length as usize] = *state;
    self.length += 1;
    State {
      path_index: state.path_index,
      glob_index: state.glob_index + 1,
      capture_index: state.capture_index + 1,
      ..State::default()
    }
  }

  #[inline(always)]
  fn pop(&mut self, state: &State, captures: &mut Option<&mut Vec<Capture>>) -> State {
    self.length -= 1;
    let mut state = State {
      path_index: self.longest_brace_match as usize,
      glob_index: state.glob_index,
      // But restore star state if needed later.
      wildcard: self.stack[self.length as usize].wildcard,
      globstar: self.stack[self.length as usize].globstar,
      capture_index: self.stack[self.length as usize].capture_index,
    };
    if self.length == 0 {
      self.longest_brace_match = 0;
    }
    state.extend_capture(captures);
    if let Some(captures) = captures {
      state.capture_index = captures.len();
    }

    state
  }

  #[inline(always)]
  fn last(&self) -> &State {
    &self.stack[self.length as usize - 1]
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn basic() {
    assert!(glob_match("abc", "abc"));
    assert!(glob_match("*", "abc"));
    assert!(glob_match("*", ""));
    assert!(glob_match("**", ""));
    assert!(glob_match("*c", "abc"));
    assert!(!glob_match("*b", "abc"));
    assert!(glob_match("a*", "abc"));
    assert!(!glob_match("b*", "abc"));
    assert!(glob_match("a*", "a"));
    assert!(glob_match("*a", "a"));
    assert!(glob_match("a*b*c*d*e*", "axbxcxdxe"));
    assert!(glob_match("a*b*c*d*e*", "axbxcxdxexxx"));
    assert!(glob_match("a*b?c*x", "abxbbxdbxebxczzx"));
    assert!(!glob_match("a*b?c*x", "abxbbxdbxebxczzy"));

    assert!(glob_match("a/*/test", "a/foo/test"));
    assert!(!glob_match("a/*/test", "a/foo/bar/test"));
    assert!(glob_match("a/**/test", "a/foo/test"));
    assert!(glob_match("a/**/test", "a/foo/bar/test"));
    assert!(glob_match("a/**/b/c", "a/foo/bar/b/c"));
    assert!(glob_match("a\\*b", "a*b"));
    assert!(!glob_match("a\\*b", "axb"));

    assert!(glob_match("[abc]", "a"));
    assert!(glob_match("[abc]", "b"));
    assert!(glob_match("[abc]", "c"));
    assert!(!glob_match("[abc]", "d"));
    assert!(glob_match("x[abc]x", "xax"));
    assert!(glob_match("x[abc]x", "xbx"));
    assert!(glob_match("x[abc]x", "xcx"));
    assert!(!glob_match("x[abc]x", "xdx"));
    assert!(!glob_match("x[abc]x", "xay"));
    assert!(glob_match("[?]", "?"));
    assert!(!glob_match("[?]", "a"));
    assert!(glob_match("[*]", "*"));
    assert!(!glob_match("[*]", "a"));

    assert!(glob_match("[a-cx]", "a"));
    assert!(glob_match("[a-cx]", "b"));
    assert!(glob_match("[a-cx]", "c"));
    assert!(!glob_match("[a-cx]", "d"));
    assert!(glob_match("[a-cx]", "x"));

    assert!(!glob_match("[^abc]", "a"));
    assert!(!glob_match("[^abc]", "b"));
    assert!(!glob_match("[^abc]", "c"));
    assert!(glob_match("[^abc]", "d"));
    assert!(!glob_match("[!abc]", "a"));
    assert!(!glob_match("[!abc]", "b"));
    assert!(!glob_match("[!abc]", "c"));
    assert!(glob_match("[!abc]", "d"));
    assert!(glob_match("[\\!]", "!"));

    assert!(glob_match("a*b*[cy]*d*e*", "axbxcxdxexxx"));
    assert!(glob_match("a*b*[cy]*d*e*", "axbxyxdxexxx"));
    assert!(glob_match("a*b*[cy]*d*e*", "axbxxxyxdxexxx"));

    assert!(glob_match("test.{jpg,png}", "test.jpg"));
    assert!(glob_match("test.{jpg,png}", "test.png"));
    assert!(glob_match("test.{j*g,p*g}", "test.jpg"));
    assert!(glob_match("test.{j*g,p*g}", "test.jpxxxg"));
    assert!(glob_match("test.{j*g,p*g}", "test.jxg"));
    assert!(!glob_match("test.{j*g,p*g}", "test.jnt"));
    assert!(glob_match("test.{j*g,j*c}", "test.jnc"));
    assert!(glob_match("test.{jpg,p*g}", "test.png"));
    assert!(glob_match("test.{jpg,p*g}", "test.pxg"));
    assert!(!glob_match("test.{jpg,p*g}", "test.pnt"));
    assert!(glob_match("test.{jpeg,png}", "test.jpeg"));
    assert!(!glob_match("test.{jpeg,png}", "test.jpg"));
    assert!(glob_match("test.{jpeg,png}", "test.png"));
    assert!(glob_match("test.{jp\\,g,png}", "test.jp,g"));
    assert!(!glob_match("test.{jp\\,g,png}", "test.jxg"));
    assert!(glob_match("test/{foo,bar}/baz", "test/foo/baz"));
    assert!(glob_match("test/{foo,bar}/baz", "test/bar/baz"));
    assert!(!glob_match("test/{foo,bar}/baz", "test/baz/baz"));
    assert!(glob_match("test/{foo*,bar*}/baz", "test/foooooo/baz"));
    assert!(glob_match("test/{foo*,bar*}/baz", "test/barrrrr/baz"));
    assert!(glob_match("test/{*foo,*bar}/baz", "test/xxxxfoo/baz"));
    assert!(glob_match("test/{*foo,*bar}/baz", "test/xxxxbar/baz"));
    assert!(glob_match("test/{foo/**,bar}/baz", "test/bar/baz"));
    assert!(!glob_match("test/{foo/**,bar}/baz", "test/bar/test/baz"));

    assert!(!glob_match("*.txt", "some/big/path/to/the/needle.txt"));
    assert!(glob_match(
      "some/**/needle.{js,tsx,mdx,ts,jsx,txt}",
      "some/a/bigger/path/to/the/crazy/needle.txt"
    ));
    assert!(glob_match(
      "some/**/{a,b,c}/**/needle.txt",
      "some/foo/a/bigger/path/to/the/crazy/needle.txt"
    ));
    assert!(!glob_match(
      "some/**/{a,b,c}/**/needle.txt",
      "some/foo/d/bigger/path/to/the/crazy/needle.txt"
    ));
    assert!(glob_match("a/{a{a,b},b}", "a/aa"));
    assert!(glob_match("a/{a{a,b},b}", "a/ab"));
    assert!(!glob_match("a/{a{a,b},b}", "a/ac"));
    assert!(glob_match("a/{a{a,b},b}", "a/b"));
    assert!(!glob_match("a/{a{a,b},b}", "a/c"));
    assert!(glob_match("a/{b,c[}]*}", "a/b"));
    assert!(glob_match("a/{b,c[}]*}", "a/c}xx"));
  }

  // The below tests are based on Bash and micromatch.
  // https://github.com/micromatch/picomatch/blob/master/test/bash.js
  // Converted using the following find and replace regex:
  // find: assert\(([!])?isMatch\('(.*?)', ['"](.*?)['"]\)\);
  // replace: assert!($1glob_match("$3", "$2"));

  #[test]
  fn bash() {
    assert!(!glob_match("a*", "*"));
    assert!(!glob_match("a*", "**"));
    assert!(!glob_match("a*", "\\*"));
    assert!(!glob_match("a*", "a/*"));
    assert!(!glob_match("a*", "b"));
    assert!(!glob_match("a*", "bc"));
    assert!(!glob_match("a*", "bcd"));
    assert!(!glob_match("a*", "bdir/"));
    assert!(!glob_match("a*", "Beware"));
    assert!(glob_match("a*", "a"));
    assert!(glob_match("a*", "ab"));
    assert!(glob_match("a*", "abc"));

    assert!(!glob_match("\\a*", "*"));
    assert!(!glob_match("\\a*", "**"));
    assert!(!glob_match("\\a*", "\\*"));

    assert!(glob_match("\\a*", "a"));
    assert!(!glob_match("\\a*", "a/*"));
    assert!(glob_match("\\a*", "abc"));
    assert!(glob_match("\\a*", "abd"));
    assert!(glob_match("\\a*", "abe"));
    assert!(!glob_match("\\a*", "b"));
    assert!(!glob_match("\\a*", "bb"));
    assert!(!glob_match("\\a*", "bcd"));
    assert!(!glob_match("\\a*", "bdir/"));
    assert!(!glob_match("\\a*", "Beware"));
    assert!(!glob_match("\\a*", "c"));
    assert!(!glob_match("\\a*", "ca"));
    assert!(!glob_match("\\a*", "cb"));
    assert!(!glob_match("\\a*", "d"));
    assert!(!glob_match("\\a*", "dd"));
    assert!(!glob_match("\\a*", "de"));
  }

  #[test]
  fn bash_directories() {
    assert!(!glob_match("b*/", "*"));
    assert!(!glob_match("b*/", "**"));
    assert!(!glob_match("b*/", "\\*"));
    assert!(!glob_match("b*/", "a"));
    assert!(!glob_match("b*/", "a/*"));
    assert!(!glob_match("b*/", "abc"));
    assert!(!glob_match("b*/", "abd"));
    assert!(!glob_match("b*/", "abe"));
    assert!(!glob_match("b*/", "b"));
    assert!(!glob_match("b*/", "bb"));
    assert!(!glob_match("b*/", "bcd"));
    assert!(glob_match("b*/", "bdir/"));
    assert!(!glob_match("b*/", "Beware"));
    assert!(!glob_match("b*/", "c"));
    assert!(!glob_match("b*/", "ca"));
    assert!(!glob_match("b*/", "cb"));
    assert!(!glob_match("b*/", "d"));
    assert!(!glob_match("b*/", "dd"));
    assert!(!glob_match("b*/", "de"));
  }

  #[test]
  fn bash_escaping() {
    assert!(!glob_match("\\^", "*"));
    assert!(!glob_match("\\^", "**"));
    assert!(!glob_match("\\^", "\\*"));
    assert!(!glob_match("\\^", "a"));
    assert!(!glob_match("\\^", "a/*"));
    assert!(!glob_match("\\^", "abc"));
    assert!(!glob_match("\\^", "abd"));
    assert!(!glob_match("\\^", "abe"));
    assert!(!glob_match("\\^", "b"));
    assert!(!glob_match("\\^", "bb"));
    assert!(!glob_match("\\^", "bcd"));
    assert!(!glob_match("\\^", "bdir/"));
    assert!(!glob_match("\\^", "Beware"));
    assert!(!glob_match("\\^", "c"));
    assert!(!glob_match("\\^", "ca"));
    assert!(!glob_match("\\^", "cb"));
    assert!(!glob_match("\\^", "d"));
    assert!(!glob_match("\\^", "dd"));
    assert!(!glob_match("\\^", "de"));

    assert!(glob_match("\\*", "*"));
    // assert!(glob_match("\\*", "\\*"));
    assert!(!glob_match("\\*", "**"));
    assert!(!glob_match("\\*", "a"));
    assert!(!glob_match("\\*", "a/*"));
    assert!(!glob_match("\\*", "abc"));
    assert!(!glob_match("\\*", "abd"));
    assert!(!glob_match("\\*", "abe"));
    assert!(!glob_match("\\*", "b"));
    assert!(!glob_match("\\*", "bb"));
    assert!(!glob_match("\\*", "bcd"));
    assert!(!glob_match("\\*", "bdir/"));
    assert!(!glob_match("\\*", "Beware"));
    assert!(!glob_match("\\*", "c"));
    assert!(!glob_match("\\*", "ca"));
    assert!(!glob_match("\\*", "cb"));
    assert!(!glob_match("\\*", "d"));
    assert!(!glob_match("\\*", "dd"));
    assert!(!glob_match("\\*", "de"));

    assert!(!glob_match("a\\*", "*"));
    assert!(!glob_match("a\\*", "**"));
    assert!(!glob_match("a\\*", "\\*"));
    assert!(!glob_match("a\\*", "a"));
    assert!(!glob_match("a\\*", "a/*"));
    assert!(!glob_match("a\\*", "abc"));
    assert!(!glob_match("a\\*", "abd"));
    assert!(!glob_match("a\\*", "abe"));
    assert!(!glob_match("a\\*", "b"));
    assert!(!glob_match("a\\*", "bb"));
    assert!(!glob_match("a\\*", "bcd"));
    assert!(!glob_match("a\\*", "bdir/"));
    assert!(!glob_match("a\\*", "Beware"));
    assert!(!glob_match("a\\*", "c"));
    assert!(!glob_match("a\\*", "ca"));
    assert!(!glob_match("a\\*", "cb"));
    assert!(!glob_match("a\\*", "d"));
    assert!(!glob_match("a\\*", "dd"));
    assert!(!glob_match("a\\*", "de"));

    assert!(glob_match("*q*", "aqa"));
    assert!(glob_match("*q*", "aaqaa"));
    assert!(!glob_match("*q*", "*"));
    assert!(!glob_match("*q*", "**"));
    assert!(!glob_match("*q*", "\\*"));
    assert!(!glob_match("*q*", "a"));
    assert!(!glob_match("*q*", "a/*"));
    assert!(!glob_match("*q*", "abc"));
    assert!(!glob_match("*q*", "abd"));
    assert!(!glob_match("*q*", "abe"));
    assert!(!glob_match("*q*", "b"));
    assert!(!glob_match("*q*", "bb"));
    assert!(!glob_match("*q*", "bcd"));
    assert!(!glob_match("*q*", "bdir/"));
    assert!(!glob_match("*q*", "Beware"));
    assert!(!glob_match("*q*", "c"));
    assert!(!glob_match("*q*", "ca"));
    assert!(!glob_match("*q*", "cb"));
    assert!(!glob_match("*q*", "d"));
    assert!(!glob_match("*q*", "dd"));
    assert!(!glob_match("*q*", "de"));

    assert!(glob_match("\\**", "*"));
    assert!(glob_match("\\**", "**"));
    assert!(!glob_match("\\**", "\\*"));
    assert!(!glob_match("\\**", "a"));
    assert!(!glob_match("\\**", "a/*"));
    assert!(!glob_match("\\**", "abc"));
    assert!(!glob_match("\\**", "abd"));
    assert!(!glob_match("\\**", "abe"));
    assert!(!glob_match("\\**", "b"));
    assert!(!glob_match("\\**", "bb"));
    assert!(!glob_match("\\**", "bcd"));
    assert!(!glob_match("\\**", "bdir/"));
    assert!(!glob_match("\\**", "Beware"));
    assert!(!glob_match("\\**", "c"));
    assert!(!glob_match("\\**", "ca"));
    assert!(!glob_match("\\**", "cb"));
    assert!(!glob_match("\\**", "d"));
    assert!(!glob_match("\\**", "dd"));
    assert!(!glob_match("\\**", "de"));
  }

  #[test]
  fn bash_classes() {
    assert!(!glob_match("a*[^c]", "*"));
    assert!(!glob_match("a*[^c]", "**"));
    assert!(!glob_match("a*[^c]", "\\*"));
    assert!(!glob_match("a*[^c]", "a"));
    assert!(!glob_match("a*[^c]", "a/*"));
    assert!(!glob_match("a*[^c]", "abc"));
    assert!(glob_match("a*[^c]", "abd"));
    assert!(glob_match("a*[^c]", "abe"));
    assert!(!glob_match("a*[^c]", "b"));
    assert!(!glob_match("a*[^c]", "bb"));
    assert!(!glob_match("a*[^c]", "bcd"));
    assert!(!glob_match("a*[^c]", "bdir/"));
    assert!(!glob_match("a*[^c]", "Beware"));
    assert!(!glob_match("a*[^c]", "c"));
    assert!(!glob_match("a*[^c]", "ca"));
    assert!(!glob_match("a*[^c]", "cb"));
    assert!(!glob_match("a*[^c]", "d"));
    assert!(!glob_match("a*[^c]", "dd"));
    assert!(!glob_match("a*[^c]", "de"));
    assert!(!glob_match("a*[^c]", "baz"));
    assert!(!glob_match("a*[^c]", "bzz"));
    assert!(!glob_match("a*[^c]", "BZZ"));
    assert!(!glob_match("a*[^c]", "beware"));
    assert!(!glob_match("a*[^c]", "BewAre"));

    assert!(glob_match("a[X-]b", "a-b"));
    assert!(glob_match("a[X-]b", "aXb"));

    assert!(!glob_match("[a-y]*[^c]", "*"));
    assert!(glob_match("[a-y]*[^c]", "a*"));
    assert!(!glob_match("[a-y]*[^c]", "**"));
    assert!(!glob_match("[a-y]*[^c]", "\\*"));
    assert!(!glob_match("[a-y]*[^c]", "a"));
    assert!(glob_match("[a-y]*[^c]", "a123b"));
    assert!(!glob_match("[a-y]*[^c]", "a123c"));
    assert!(glob_match("[a-y]*[^c]", "ab"));
    assert!(!glob_match("[a-y]*[^c]", "a/*"));
    assert!(!glob_match("[a-y]*[^c]", "abc"));
    assert!(glob_match("[a-y]*[^c]", "abd"));
    assert!(glob_match("[a-y]*[^c]", "abe"));
    assert!(!glob_match("[a-y]*[^c]", "b"));
    assert!(glob_match("[a-y]*[^c]", "bd"));
    assert!(glob_match("[a-y]*[^c]", "bb"));
    assert!(glob_match("[a-y]*[^c]", "bcd"));
    assert!(glob_match("[a-y]*[^c]", "bdir/"));
    assert!(!glob_match("[a-y]*[^c]", "Beware"));
    assert!(!glob_match("[a-y]*[^c]", "c"));
    assert!(glob_match("[a-y]*[^c]", "ca"));
    assert!(glob_match("[a-y]*[^c]", "cb"));
    assert!(!glob_match("[a-y]*[^c]", "d"));
    assert!(glob_match("[a-y]*[^c]", "dd"));
    assert!(glob_match("[a-y]*[^c]", "dd"));
    assert!(glob_match("[a-y]*[^c]", "dd"));
    assert!(glob_match("[a-y]*[^c]", "de"));
    assert!(glob_match("[a-y]*[^c]", "baz"));
    assert!(glob_match("[a-y]*[^c]", "bzz"));
    assert!(glob_match("[a-y]*[^c]", "bzz"));
    // assert(!isMatch('bzz', '[a-y]*[^c]', { regex: true }));
    assert!(!glob_match("[a-y]*[^c]", "BZZ"));
    assert!(glob_match("[a-y]*[^c]", "beware"));
    assert!(!glob_match("[a-y]*[^c]", "BewAre"));

    assert!(glob_match("a\\*b/*", "a*b/ooo"));
    assert!(glob_match("a\\*?/*", "a*b/ooo"));

    assert!(!glob_match("a[b]c", "*"));
    assert!(!glob_match("a[b]c", "**"));
    assert!(!glob_match("a[b]c", "\\*"));
    assert!(!glob_match("a[b]c", "a"));
    assert!(!glob_match("a[b]c", "a/*"));
    assert!(glob_match("a[b]c", "abc"));
    assert!(!glob_match("a[b]c", "abd"));
    assert!(!glob_match("a[b]c", "abe"));
    assert!(!glob_match("a[b]c", "b"));
    assert!(!glob_match("a[b]c", "bb"));
    assert!(!glob_match("a[b]c", "bcd"));
    assert!(!glob_match("a[b]c", "bdir/"));
    assert!(!glob_match("a[b]c", "Beware"));
    assert!(!glob_match("a[b]c", "c"));
    assert!(!glob_match("a[b]c", "ca"));
    assert!(!glob_match("a[b]c", "cb"));
    assert!(!glob_match("a[b]c", "d"));
    assert!(!glob_match("a[b]c", "dd"));
    assert!(!glob_match("a[b]c", "de"));
    assert!(!glob_match("a[b]c", "baz"));
    assert!(!glob_match("a[b]c", "bzz"));
    assert!(!glob_match("a[b]c", "BZZ"));
    assert!(!glob_match("a[b]c", "beware"));
    assert!(!glob_match("a[b]c", "BewAre"));

    assert!(!glob_match("a[\"b\"]c", "*"));
    assert!(!glob_match("a[\"b\"]c", "**"));
    assert!(!glob_match("a[\"b\"]c", "\\*"));
    assert!(!glob_match("a[\"b\"]c", "a"));
    assert!(!glob_match("a[\"b\"]c", "a/*"));
    assert!(glob_match("a[\"b\"]c", "abc"));
    assert!(!glob_match("a[\"b\"]c", "abd"));
    assert!(!glob_match("a[\"b\"]c", "abe"));
    assert!(!glob_match("a[\"b\"]c", "b"));
    assert!(!glob_match("a[\"b\"]c", "bb"));
    assert!(!glob_match("a[\"b\"]c", "bcd"));
    assert!(!glob_match("a[\"b\"]c", "bdir/"));
    assert!(!glob_match("a[\"b\"]c", "Beware"));
    assert!(!glob_match("a[\"b\"]c", "c"));
    assert!(!glob_match("a[\"b\"]c", "ca"));
    assert!(!glob_match("a[\"b\"]c", "cb"));
    assert!(!glob_match("a[\"b\"]c", "d"));
    assert!(!glob_match("a[\"b\"]c", "dd"));
    assert!(!glob_match("a[\"b\"]c", "de"));
    assert!(!glob_match("a[\"b\"]c", "baz"));
    assert!(!glob_match("a[\"b\"]c", "bzz"));
    assert!(!glob_match("a[\"b\"]c", "BZZ"));
    assert!(!glob_match("a[\"b\"]c", "beware"));
    assert!(!glob_match("a[\"b\"]c", "BewAre"));

    assert!(!glob_match("a[\\\\b]c", "*"));
    assert!(!glob_match("a[\\\\b]c", "**"));
    assert!(!glob_match("a[\\\\b]c", "\\*"));
    assert!(!glob_match("a[\\\\b]c", "a"));
    assert!(!glob_match("a[\\\\b]c", "a/*"));
    assert!(glob_match("a[\\\\b]c", "abc"));
    assert!(!glob_match("a[\\\\b]c", "abd"));
    assert!(!glob_match("a[\\\\b]c", "abe"));
    assert!(!glob_match("a[\\\\b]c", "b"));
    assert!(!glob_match("a[\\\\b]c", "bb"));
    assert!(!glob_match("a[\\\\b]c", "bcd"));
    assert!(!glob_match("a[\\\\b]c", "bdir/"));
    assert!(!glob_match("a[\\\\b]c", "Beware"));
    assert!(!glob_match("a[\\\\b]c", "c"));
    assert!(!glob_match("a[\\\\b]c", "ca"));
    assert!(!glob_match("a[\\\\b]c", "cb"));
    assert!(!glob_match("a[\\\\b]c", "d"));
    assert!(!glob_match("a[\\\\b]c", "dd"));
    assert!(!glob_match("a[\\\\b]c", "de"));
    assert!(!glob_match("a[\\\\b]c", "baz"));
    assert!(!glob_match("a[\\\\b]c", "bzz"));
    assert!(!glob_match("a[\\\\b]c", "BZZ"));
    assert!(!glob_match("a[\\\\b]c", "beware"));
    assert!(!glob_match("a[\\\\b]c", "BewAre"));

    assert!(!glob_match("a[\\b]c", "*"));
    assert!(!glob_match("a[\\b]c", "**"));
    assert!(!glob_match("a[\\b]c", "\\*"));
    assert!(!glob_match("a[\\b]c", "a"));
    assert!(!glob_match("a[\\b]c", "a/*"));
    assert!(!glob_match("a[\\b]c", "abc"));
    assert!(!glob_match("a[\\b]c", "abd"));
    assert!(!glob_match("a[\\b]c", "abe"));
    assert!(!glob_match("a[\\b]c", "b"));
    assert!(!glob_match("a[\\b]c", "bb"));
    assert!(!glob_match("a[\\b]c", "bcd"));
    assert!(!glob_match("a[\\b]c", "bdir/"));
    assert!(!glob_match("a[\\b]c", "Beware"));
    assert!(!glob_match("a[\\b]c", "c"));
    assert!(!glob_match("a[\\b]c", "ca"));
    assert!(!glob_match("a[\\b]c", "cb"));
    assert!(!glob_match("a[\\b]c", "d"));
    assert!(!glob_match("a[\\b]c", "dd"));
    assert!(!glob_match("a[\\b]c", "de"));
    assert!(!glob_match("a[\\b]c", "baz"));
    assert!(!glob_match("a[\\b]c", "bzz"));
    assert!(!glob_match("a[\\b]c", "BZZ"));
    assert!(!glob_match("a[\\b]c", "beware"));
    assert!(!glob_match("a[\\b]c", "BewAre"));

    assert!(!glob_match("a[b-d]c", "*"));
    assert!(!glob_match("a[b-d]c", "**"));
    assert!(!glob_match("a[b-d]c", "\\*"));
    assert!(!glob_match("a[b-d]c", "a"));
    assert!(!glob_match("a[b-d]c", "a/*"));
    assert!(glob_match("a[b-d]c", "abc"));
    assert!(!glob_match("a[b-d]c", "abd"));
    assert!(!glob_match("a[b-d]c", "abe"));
    assert!(!glob_match("a[b-d]c", "b"));
    assert!(!glob_match("a[b-d]c", "bb"));
    assert!(!glob_match("a[b-d]c", "bcd"));
    assert!(!glob_match("a[b-d]c", "bdir/"));
    assert!(!glob_match("a[b-d]c", "Beware"));
    assert!(!glob_match("a[b-d]c", "c"));
    assert!(!glob_match("a[b-d]c", "ca"));
    assert!(!glob_match("a[b-d]c", "cb"));
    assert!(!glob_match("a[b-d]c", "d"));
    assert!(!glob_match("a[b-d]c", "dd"));
    assert!(!glob_match("a[b-d]c", "de"));
    assert!(!glob_match("a[b-d]c", "baz"));
    assert!(!glob_match("a[b-d]c", "bzz"));
    assert!(!glob_match("a[b-d]c", "BZZ"));
    assert!(!glob_match("a[b-d]c", "beware"));
    assert!(!glob_match("a[b-d]c", "BewAre"));

    assert!(!glob_match("a?c", "*"));
    assert!(!glob_match("a?c", "**"));
    assert!(!glob_match("a?c", "\\*"));
    assert!(!glob_match("a?c", "a"));
    assert!(!glob_match("a?c", "a/*"));
    assert!(glob_match("a?c", "abc"));
    assert!(!glob_match("a?c", "abd"));
    assert!(!glob_match("a?c", "abe"));
    assert!(!glob_match("a?c", "b"));
    assert!(!glob_match("a?c", "bb"));
    assert!(!glob_match("a?c", "bcd"));
    assert!(!glob_match("a?c", "bdir/"));
    assert!(!glob_match("a?c", "Beware"));
    assert!(!glob_match("a?c", "c"));
    assert!(!glob_match("a?c", "ca"));
    assert!(!glob_match("a?c", "cb"));
    assert!(!glob_match("a?c", "d"));
    assert!(!glob_match("a?c", "dd"));
    assert!(!glob_match("a?c", "de"));
    assert!(!glob_match("a?c", "baz"));
    assert!(!glob_match("a?c", "bzz"));
    assert!(!glob_match("a?c", "BZZ"));
    assert!(!glob_match("a?c", "beware"));
    assert!(!glob_match("a?c", "BewAre"));

    assert!(glob_match("*/man*/bash.*", "man/man1/bash.1"));

    assert!(glob_match("[^a-c]*", "*"));
    assert!(glob_match("[^a-c]*", "**"));
    assert!(!glob_match("[^a-c]*", "a"));
    assert!(!glob_match("[^a-c]*", "a/*"));
    assert!(!glob_match("[^a-c]*", "abc"));
    assert!(!glob_match("[^a-c]*", "abd"));
    assert!(!glob_match("[^a-c]*", "abe"));
    assert!(!glob_match("[^a-c]*", "b"));
    assert!(!glob_match("[^a-c]*", "bb"));
    assert!(!glob_match("[^a-c]*", "bcd"));
    assert!(!glob_match("[^a-c]*", "bdir/"));
    assert!(glob_match("[^a-c]*", "Beware"));
    assert!(glob_match("[^a-c]*", "Beware"));
    assert!(!glob_match("[^a-c]*", "c"));
    assert!(!glob_match("[^a-c]*", "ca"));
    assert!(!glob_match("[^a-c]*", "cb"));
    assert!(glob_match("[^a-c]*", "d"));
    assert!(glob_match("[^a-c]*", "dd"));
    assert!(glob_match("[^a-c]*", "de"));
    assert!(!glob_match("[^a-c]*", "baz"));
    assert!(!glob_match("[^a-c]*", "bzz"));
    assert!(glob_match("[^a-c]*", "BZZ"));
    assert!(!glob_match("[^a-c]*", "beware"));
    assert!(glob_match("[^a-c]*", "BewAre"));
  }

  #[test]
  fn bash_wildmatch() {
    assert!(!glob_match("a[]-]b", "aab"));
    assert!(!glob_match("[ten]", "ten"));
    assert!(glob_match("]", "]"));
    assert!(glob_match("a[]-]b", "a-b"));
    assert!(glob_match("a[]-]b", "a]b"));
    assert!(glob_match("a[]]b", "a]b"));
    assert!(glob_match("a[\\]a\\-]b", "aab"));
    assert!(glob_match("t[a-g]n", "ten"));
    assert!(glob_match("t[^a-g]n", "ton"));
  }

  #[test]
  fn bash_slashmatch() {
    // assert!(!glob_match("f[^eiu][^eiu][^eiu][^eiu][^eiu]r", "foo/bar"));
    assert!(glob_match("foo[/]bar", "foo/bar"));
    assert!(glob_match("f[^eiu][^eiu][^eiu][^eiu][^eiu]r", "foo-bar"));
  }

  #[test]
  fn bash_extra_stars() {
    assert!(!glob_match("a**c", "bbc"));
    assert!(glob_match("a**c", "abc"));
    assert!(!glob_match("a**c", "bbd"));

    assert!(!glob_match("a***c", "bbc"));
    assert!(glob_match("a***c", "abc"));
    assert!(!glob_match("a***c", "bbd"));

    assert!(!glob_match("a*****?c", "bbc"));
    assert!(glob_match("a*****?c", "abc"));
    assert!(!glob_match("a*****?c", "bbc"));

    assert!(glob_match("?*****??", "bbc"));
    assert!(glob_match("?*****??", "abc"));

    assert!(glob_match("*****??", "bbc"));
    assert!(glob_match("*****??", "abc"));

    assert!(glob_match("?*****?c", "bbc"));
    assert!(glob_match("?*****?c", "abc"));

    assert!(glob_match("?***?****c", "bbc"));
    assert!(glob_match("?***?****c", "abc"));
    assert!(!glob_match("?***?****c", "bbd"));

    assert!(glob_match("?***?****?", "bbc"));
    assert!(glob_match("?***?****?", "abc"));

    assert!(glob_match("?***?****", "bbc"));
    assert!(glob_match("?***?****", "abc"));

    assert!(glob_match("*******c", "bbc"));
    assert!(glob_match("*******c", "abc"));

    assert!(glob_match("*******?", "bbc"));
    assert!(glob_match("*******?", "abc"));

    assert!(glob_match("a*cd**?**??k", "abcdecdhjk"));
    assert!(glob_match("a**?**cd**?**??k", "abcdecdhjk"));
    assert!(glob_match("a**?**cd**?**??k***", "abcdecdhjk"));
    assert!(glob_match("a**?**cd**?**??***k", "abcdecdhjk"));
    assert!(glob_match("a**?**cd**?**??***k**", "abcdecdhjk"));
    assert!(glob_match("a****c**?**??*****", "abcdecdhjk"));
  }

  #[test]
  fn stars() {
    assert!(!glob_match("*.js", "a/b/c/z.js"));
    assert!(!glob_match("*.js", "a/b/z.js"));
    assert!(!glob_match("*.js", "a/z.js"));
    assert!(glob_match("*.js", "z.js"));

    // assert!(!glob_match("*/*", "a/.ab"));
    // assert!(!glob_match("*", ".ab"));

    assert!(glob_match("z*.js", "z.js"));
    assert!(glob_match("*/*", "a/z"));
    assert!(glob_match("*/z*.js", "a/z.js"));
    assert!(glob_match("a/z*.js", "a/z.js"));

    assert!(glob_match("*", "ab"));
    assert!(glob_match("*", "abc"));

    assert!(!glob_match("f*", "bar"));
    assert!(!glob_match("*r", "foo"));
    assert!(!glob_match("b*", "foo"));
    assert!(!glob_match("*", "foo/bar"));
    assert!(glob_match("*c", "abc"));
    assert!(glob_match("a*", "abc"));
    assert!(glob_match("a*c", "abc"));
    assert!(glob_match("*r", "bar"));
    assert!(glob_match("b*", "bar"));
    assert!(glob_match("f*", "foo"));

    assert!(glob_match("*abc*", "one abc two"));
    assert!(glob_match("a*b", "a         b"));

    assert!(!glob_match("*a*", "foo"));
    assert!(glob_match("*a*", "bar"));
    assert!(glob_match("*abc*", "oneabctwo"));
    assert!(!glob_match("*-bc-*", "a-b.c-d"));
    assert!(glob_match("*-*.*-*", "a-b.c-d"));
    assert!(glob_match("*-b*c-*", "a-b.c-d"));
    assert!(glob_match("*-b.c-*", "a-b.c-d"));
    assert!(glob_match("*.*", "a-b.c-d"));
    assert!(glob_match("*.*-*", "a-b.c-d"));
    assert!(glob_match("*.*-d", "a-b.c-d"));
    assert!(glob_match("*.c-*", "a-b.c-d"));
    assert!(glob_match("*b.*d", "a-b.c-d"));
    assert!(glob_match("a*.c*", "a-b.c-d"));
    assert!(glob_match("a-*.*-d", "a-b.c-d"));
    assert!(glob_match("*.*", "a.b"));
    assert!(glob_match("*.b", "a.b"));
    assert!(glob_match("a.*", "a.b"));
    assert!(glob_match("a.b", "a.b"));

    assert!(!glob_match("**-bc-**", "a-b.c-d"));
    assert!(glob_match("**-**.**-**", "a-b.c-d"));
    assert!(glob_match("**-b**c-**", "a-b.c-d"));
    assert!(glob_match("**-b.c-**", "a-b.c-d"));
    assert!(glob_match("**.**", "a-b.c-d"));
    assert!(glob_match("**.**-**", "a-b.c-d"));
    assert!(glob_match("**.**-d", "a-b.c-d"));
    assert!(glob_match("**.c-**", "a-b.c-d"));
    assert!(glob_match("**b.**d", "a-b.c-d"));
    assert!(glob_match("a**.c**", "a-b.c-d"));
    assert!(glob_match("a-**.**-d", "a-b.c-d"));
    assert!(glob_match("**.**", "a.b"));
    assert!(glob_match("**.b", "a.b"));
    assert!(glob_match("a.**", "a.b"));
    assert!(glob_match("a.b", "a.b"));

    assert!(glob_match("*/*", "/ab"));
    assert!(glob_match(".", "."));
    assert!(!glob_match("a/", "a/.b"));
    assert!(glob_match("/*", "/ab"));
    assert!(glob_match("/??", "/ab"));
    assert!(glob_match("/?b", "/ab"));
    assert!(glob_match("/*", "/cd"));
    assert!(glob_match("a", "a"));
    assert!(glob_match("a/.*", "a/.b"));
    assert!(glob_match("?/?", "a/b"));
    assert!(glob_match("a/**/j/**/z/*.md", "a/b/c/d/e/j/n/p/o/z/c.md"));
    assert!(glob_match("a/**/z/*.md", "a/b/c/d/e/z/c.md"));
    assert!(glob_match("a/b/c/*.md", "a/b/c/xyz.md"));
    assert!(glob_match("a/b/c/*.md", "a/b/c/xyz.md"));
    assert!(glob_match("a/*/z/.a", "a/b/z/.a"));
    assert!(!glob_match("bz", "a/b/z/.a"));
    assert!(glob_match("a/**/c/*.md", "a/bb.bb/aa/b.b/aa/c/xyz.md"));
    assert!(glob_match("a/**/c/*.md", "a/bb.bb/aa/bb/aa/c/xyz.md"));
    assert!(glob_match("a/*/c/*.md", "a/bb.bb/c/xyz.md"));
    assert!(glob_match("a/*/c/*.md", "a/bb/c/xyz.md"));
    assert!(glob_match("a/*/c/*.md", "a/bbbb/c/xyz.md"));
    assert!(glob_match("*", "aaa"));
    assert!(glob_match("*", "ab"));
    assert!(glob_match("ab", "ab"));

    assert!(!glob_match("*/*/*", "aaa"));
    assert!(!glob_match("*/*/*", "aaa/bb/aa/rr"));
    assert!(!glob_match("aaa*", "aaa/bba/ccc"));
    // assert!(!glob_match("aaa**", "aaa/bba/ccc"));
    assert!(!glob_match("aaa/*", "aaa/bba/ccc"));
    assert!(!glob_match("aaa/*ccc", "aaa/bba/ccc"));
    assert!(!glob_match("aaa/*z", "aaa/bba/ccc"));
    assert!(!glob_match("*/*/*", "aaa/bbb"));
    assert!(!glob_match("*/*jk*/*i", "ab/zzz/ejkl/hi"));
    assert!(glob_match("*/*/*", "aaa/bba/ccc"));
    assert!(glob_match("aaa/**", "aaa/bba/ccc"));
    assert!(glob_match("aaa/*", "aaa/bbb"));
    assert!(glob_match("*/*z*/*/*i", "ab/zzz/ejkl/hi"));
    assert!(glob_match("*j*i", "abzzzejklhi"));

    assert!(glob_match("*", "a"));
    assert!(glob_match("*", "b"));
    assert!(!glob_match("*", "a/a"));
    assert!(!glob_match("*", "a/a/a"));
    assert!(!glob_match("*", "a/a/b"));
    assert!(!glob_match("*", "a/a/a/a"));
    assert!(!glob_match("*", "a/a/a/a/a"));

    assert!(!glob_match("*/*", "a"));
    assert!(glob_match("*/*", "a/a"));
    assert!(!glob_match("*/*", "a/a/a"));

    assert!(!glob_match("*/*/*", "a"));
    assert!(!glob_match("*/*/*", "a/a"));
    assert!(glob_match("*/*/*", "a/a/a"));
    assert!(!glob_match("*/*/*", "a/a/a/a"));

    assert!(!glob_match("*/*/*/*", "a"));
    assert!(!glob_match("*/*/*/*", "a/a"));
    assert!(!glob_match("*/*/*/*", "a/a/a"));
    assert!(glob_match("*/*/*/*", "a/a/a/a"));
    assert!(!glob_match("*/*/*/*", "a/a/a/a/a"));

    assert!(!glob_match("*/*/*/*/*", "a"));
    assert!(!glob_match("*/*/*/*/*", "a/a"));
    assert!(!glob_match("*/*/*/*/*", "a/a/a"));
    assert!(!glob_match("*/*/*/*/*", "a/a/b"));
    assert!(!glob_match("*/*/*/*/*", "a/a/a/a"));
    assert!(glob_match("*/*/*/*/*", "a/a/a/a/a"));
    assert!(!glob_match("*/*/*/*/*", "a/a/a/a/a/a"));

    assert!(!glob_match("a/*", "a"));
    assert!(glob_match("a/*", "a/a"));
    assert!(!glob_match("a/*", "a/a/a"));
    assert!(!glob_match("a/*", "a/a/a/a"));
    assert!(!glob_match("a/*", "a/a/a/a/a"));

    assert!(!glob_match("a/*/*", "a"));
    assert!(!glob_match("a/*/*", "a/a"));
    assert!(glob_match("a/*/*", "a/a/a"));
    assert!(!glob_match("a/*/*", "b/a/a"));
    assert!(!glob_match("a/*/*", "a/a/a/a"));
    assert!(!glob_match("a/*/*", "a/a/a/a/a"));

    assert!(!glob_match("a/*/*/*", "a"));
    assert!(!glob_match("a/*/*/*", "a/a"));
    assert!(!glob_match("a/*/*/*", "a/a/a"));
    assert!(glob_match("a/*/*/*", "a/a/a/a"));
    assert!(!glob_match("a/*/*/*", "a/a/a/a/a"));

    assert!(!glob_match("a/*/*/*/*", "a"));
    assert!(!glob_match("a/*/*/*/*", "a/a"));
    assert!(!glob_match("a/*/*/*/*", "a/a/a"));
    assert!(!glob_match("a/*/*/*/*", "a/a/b"));
    assert!(!glob_match("a/*/*/*/*", "a/a/a/a"));
    assert!(glob_match("a/*/*/*/*", "a/a/a/a/a"));

    assert!(!glob_match("a/*/a", "a"));
    assert!(!glob_match("a/*/a", "a/a"));
    assert!(glob_match("a/*/a", "a/a/a"));
    assert!(!glob_match("a/*/a", "a/a/b"));
    assert!(!glob_match("a/*/a", "a/a/a/a"));
    assert!(!glob_match("a/*/a", "a/a/a/a/a"));

    assert!(!glob_match("a/*/b", "a"));
    assert!(!glob_match("a/*/b", "a/a"));
    assert!(!glob_match("a/*/b", "a/a/a"));
    assert!(glob_match("a/*/b", "a/a/b"));
    assert!(!glob_match("a/*/b", "a/a/a/a"));
    assert!(!glob_match("a/*/b", "a/a/a/a/a"));

    assert!(!glob_match("*/**/a", "a"));
    assert!(!glob_match("*/**/a", "a/a/b"));
    assert!(glob_match("*/**/a", "a/a"));
    assert!(glob_match("*/**/a", "a/a/a"));
    assert!(glob_match("*/**/a", "a/a/a/a"));
    assert!(glob_match("*/**/a", "a/a/a/a/a"));

    assert!(!glob_match("*/", "a"));
    assert!(!glob_match("*/*", "a"));
    assert!(!glob_match("a/*", "a"));
    // assert!(!glob_match("*/*", "a/"));
    // assert!(!glob_match("a/*", "a/"));
    assert!(!glob_match("*", "a/a"));
    assert!(!glob_match("*/", "a/a"));
    assert!(!glob_match("*/", "a/x/y"));
    assert!(!glob_match("*/*", "a/x/y"));
    assert!(!glob_match("a/*", "a/x/y"));
    // assert!(glob_match("*", "a/"));
    assert!(glob_match("*", "a"));
    assert!(glob_match("*/", "a/"));
    assert!(glob_match("*{,/}", "a/"));
    assert!(glob_match("*/*", "a/a"));
    assert!(glob_match("a/*", "a/a"));

    assert!(!glob_match("a/**/*.txt", "a.txt"));
    assert!(glob_match("a/**/*.txt", "a/x/y.txt"));
    assert!(!glob_match("a/**/*.txt", "a/x/y/z"));

    assert!(!glob_match("a/*.txt", "a.txt"));
    assert!(glob_match("a/*.txt", "a/b.txt"));
    assert!(!glob_match("a/*.txt", "a/x/y.txt"));
    assert!(!glob_match("a/*.txt", "a/x/y/z"));

    assert!(glob_match("a*.txt", "a.txt"));
    assert!(!glob_match("a*.txt", "a/b.txt"));
    assert!(!glob_match("a*.txt", "a/x/y.txt"));
    assert!(!glob_match("a*.txt", "a/x/y/z"));

    assert!(glob_match("*.txt", "a.txt"));
    assert!(!glob_match("*.txt", "a/b.txt"));
    assert!(!glob_match("*.txt", "a/x/y.txt"));
    assert!(!glob_match("*.txt", "a/x/y/z"));

    assert!(!glob_match("a*", "a/b"));
    assert!(!glob_match("a/**/b", "a/a/bb"));
    assert!(!glob_match("a/**/b", "a/bb"));

    assert!(!glob_match("*/**", "foo"));
    assert!(!glob_match("**/", "foo/bar"));
    assert!(!glob_match("**/*/", "foo/bar"));
    assert!(!glob_match("*/*/", "foo/bar"));

    assert!(glob_match("**/..", "/home/foo/.."));
    assert!(glob_match("**/a", "a"));
    assert!(glob_match("**", "a/a"));
    assert!(glob_match("a/**", "a/a"));
    assert!(glob_match("a/**", "a/"));
    // assert!(glob_match("a/**", "a"));
    assert!(!glob_match("**/", "a/a"));
    // assert!(glob_match("**/a/**", "a"));
    // assert!(glob_match("a/**", "a"));
    assert!(!glob_match("**/", "a/a"));
    assert!(glob_match("*/**/a", "a/a"));
    // assert!(glob_match("a/**", "a"));
    assert!(glob_match("*/**", "foo/"));
    assert!(glob_match("**/*", "foo/bar"));
    assert!(glob_match("*/*", "foo/bar"));
    assert!(glob_match("*/**", "foo/bar"));
    assert!(glob_match("**/", "foo/bar/"));
    // assert!(glob_match("**/*", "foo/bar/"));
    assert!(glob_match("**/*/", "foo/bar/"));
    assert!(glob_match("*/**", "foo/bar/"));
    assert!(glob_match("*/*/", "foo/bar/"));

    assert!(!glob_match("*/foo", "bar/baz/foo"));
    assert!(!glob_match("**/bar/*", "deep/foo/bar"));
    assert!(!glob_match("*/bar/**", "deep/foo/bar/baz/x"));
    assert!(!glob_match("/*", "ef"));
    assert!(!glob_match("foo?bar", "foo/bar"));
    assert!(!glob_match("**/bar*", "foo/bar/baz"));
    // assert!(!glob_match("**/bar**", "foo/bar/baz"));
    assert!(!glob_match("foo**bar", "foo/baz/bar"));
    assert!(!glob_match("foo*bar", "foo/baz/bar"));
    // assert!(glob_match("foo/**", "foo"));
    assert!(glob_match("/*", "/ab"));
    assert!(glob_match("/*", "/cd"));
    assert!(glob_match("/*", "/ef"));
    assert!(glob_match("a/**/j/**/z/*.md", "a/b/j/c/z/x.md"));
    assert!(glob_match("a/**/j/**/z/*.md", "a/j/z/x.md"));

    assert!(glob_match("**/foo", "bar/baz/foo"));
    assert!(glob_match("**/bar/*", "deep/foo/bar/baz"));
    assert!(glob_match("**/bar/**", "deep/foo/bar/baz/"));
    assert!(glob_match("**/bar/*/*", "deep/foo/bar/baz/x"));
    assert!(glob_match("foo/**/**/bar", "foo/b/a/z/bar"));
    assert!(glob_match("foo/**/bar", "foo/b/a/z/bar"));
    assert!(glob_match("foo/**/**/bar", "foo/bar"));
    assert!(glob_match("foo/**/bar", "foo/bar"));
    assert!(glob_match("*/bar/**", "foo/bar/baz/x"));
    assert!(glob_match("foo/**/**/bar", "foo/baz/bar"));
    assert!(glob_match("foo/**/bar", "foo/baz/bar"));
    assert!(glob_match("**/foo", "XXX/foo"));
  }

  #[test]
  fn globstars() {
    assert!(glob_match("**/*.js", "a/b/c/d.js"));
    assert!(glob_match("**/*.js", "a/b/c.js"));
    assert!(glob_match("**/*.js", "a/b.js"));
    assert!(glob_match("a/b/**/*.js", "a/b/c/d/e/f.js"));
    assert!(glob_match("a/b/**/*.js", "a/b/c/d/e.js"));
    assert!(glob_match("a/b/c/**/*.js", "a/b/c/d.js"));
    assert!(glob_match("a/b/**/*.js", "a/b/c/d.js"));
    assert!(glob_match("a/b/**/*.js", "a/b/d.js"));
    assert!(!glob_match("a/b/**/*.js", "a/d.js"));
    assert!(!glob_match("a/b/**/*.js", "d.js"));

    assert!(!glob_match("**c", "a/b/c"));
    assert!(!glob_match("a/**c", "a/b/c"));
    assert!(!glob_match("a/**z", "a/b/c"));
    assert!(!glob_match("a/**b**/c", "a/b/c/b/c"));
    assert!(!glob_match("a/b/c**/*.js", "a/b/c/d/e.js"));
    assert!(glob_match("a/**/b/**/c", "a/b/c/b/c"));
    assert!(glob_match("a/**b**/c", "a/aba/c"));
    assert!(glob_match("a/**b**/c", "a/b/c"));
    assert!(glob_match("a/b/c**/*.js", "a/b/c/d.js"));

    assert!(!glob_match("a/**/*", "a"));
    assert!(!glob_match("a/**/**/*", "a"));
    assert!(!glob_match("a/**/**/**/*", "a"));
    assert!(!glob_match("**/a", "a/"));
    assert!(!glob_match("a/**/*", "a/"));
    assert!(!glob_match("a/**/**/*", "a/"));
    assert!(!glob_match("a/**/**/**/*", "a/"));
    assert!(!glob_match("**/a", "a/b"));
    assert!(!glob_match("a/**/j/**/z/*.md", "a/b/c/j/e/z/c.txt"));
    assert!(!glob_match("a/**/b", "a/bb"));
    assert!(!glob_match("**/a", "a/c"));
    assert!(!glob_match("**/a", "a/b"));
    assert!(!glob_match("**/a", "a/x/y"));
    assert!(!glob_match("**/a", "a/b/c/d"));
    assert!(glob_match("**", "a"));
    assert!(glob_match("**/a", "a"));
    // assert!(glob_match("a/**", "a"));
    assert!(glob_match("**", "a/"));
    assert!(glob_match("**/a/**", "a/"));
    assert!(glob_match("a/**", "a/"));
    assert!(glob_match("a/**/**", "a/"));
    assert!(glob_match("**/a", "a/a"));
    assert!(glob_match("**", "a/b"));
    assert!(glob_match("*/*", "a/b"));
    assert!(glob_match("a/**", "a/b"));
    assert!(glob_match("a/**/*", "a/b"));
    assert!(glob_match("a/**/**/*", "a/b"));
    assert!(glob_match("a/**/**/**/*", "a/b"));
    assert!(glob_match("a/**/b", "a/b"));
    assert!(glob_match("**", "a/b/c"));
    assert!(glob_match("**/*", "a/b/c"));
    assert!(glob_match("**/**", "a/b/c"));
    assert!(glob_match("*/**", "a/b/c"));
    assert!(glob_match("a/**", "a/b/c"));
    assert!(glob_match("a/**/*", "a/b/c"));
    assert!(glob_match("a/**/**/*", "a/b/c"));
    assert!(glob_match("a/**/**/**/*", "a/b/c"));
    assert!(glob_match("**", "a/b/c/d"));
    assert!(glob_match("a/**", "a/b/c/d"));
    assert!(glob_match("a/**/*", "a/b/c/d"));
    assert!(glob_match("a/**/**/*", "a/b/c/d"));
    assert!(glob_match("a/**/**/**/*", "a/b/c/d"));
    assert!(glob_match("a/b/**/c/**/*.*", "a/b/c/d.e"));
    assert!(glob_match("a/**/f/*.md", "a/b/c/d/e/f/g.md"));
    assert!(glob_match("a/**/f/**/k/*.md", "a/b/c/d/e/f/g/h/i/j/k/l.md"));
    assert!(glob_match("a/b/c/*.md", "a/b/c/def.md"));
    assert!(glob_match("a/*/c/*.md", "a/bb.bb/c/ddd.md"));
    assert!(glob_match("a/**/f/*.md", "a/bb.bb/cc/d.d/ee/f/ggg.md"));
    assert!(glob_match("a/**/f/*.md", "a/bb.bb/cc/dd/ee/f/ggg.md"));
    assert!(glob_match("a/*/c/*.md", "a/bb/c/ddd.md"));
    assert!(glob_match("a/*/c/*.md", "a/bbbb/c/ddd.md"));

    assert!(glob_match(
      "foo/bar/**/one/**/*.*",
      "foo/bar/baz/one/image.png"
    ));
    assert!(glob_match(
      "foo/bar/**/one/**/*.*",
      "foo/bar/baz/one/two/image.png"
    ));
    assert!(glob_match(
      "foo/bar/**/one/**/*.*",
      "foo/bar/baz/one/two/three/image.png"
    ));
    assert!(!glob_match("a/b/**/f", "a/b/c/d/"));
    // assert!(glob_match("a/**", "a"));
    assert!(glob_match("**", "a"));
    // assert!(glob_match("a{,/**}", "a"));
    assert!(glob_match("**", "a/"));
    assert!(glob_match("a/**", "a/"));
    assert!(glob_match("**", "a/b/c/d"));
    assert!(glob_match("**", "a/b/c/d/"));
    assert!(glob_match("**/**", "a/b/c/d/"));
    assert!(glob_match("**/b/**", "a/b/c/d/"));
    assert!(glob_match("a/b/**", "a/b/c/d/"));
    assert!(glob_match("a/b/**/", "a/b/c/d/"));
    assert!(glob_match("a/b/**/c/**/", "a/b/c/d/"));
    assert!(glob_match("a/b/**/c/**/d/", "a/b/c/d/"));
    assert!(glob_match("a/b/**/**/*.*", "a/b/c/d/e.f"));
    assert!(glob_match("a/b/**/*.*", "a/b/c/d/e.f"));
    assert!(glob_match("a/b/**/c/**/d/*.*", "a/b/c/d/e.f"));
    assert!(glob_match("a/b/**/d/**/*.*", "a/b/c/d/e.f"));
    assert!(glob_match("a/b/**/d/**/*.*", "a/b/c/d/g/e.f"));
    assert!(glob_match("a/b/**/d/**/*.*", "a/b/c/d/g/g/e.f"));
    assert!(glob_match("a/b-*/**/z.js", "a/b-c/z.js"));
    assert!(glob_match("a/b-*/**/z.js", "a/b-c/d/e/z.js"));

    assert!(glob_match("*/*", "a/b"));
    assert!(glob_match("a/b/c/*.md", "a/b/c/xyz.md"));
    assert!(glob_match("a/*/c/*.md", "a/bb.bb/c/xyz.md"));
    assert!(glob_match("a/*/c/*.md", "a/bb/c/xyz.md"));
    assert!(glob_match("a/*/c/*.md", "a/bbbb/c/xyz.md"));

    assert!(glob_match("**/*", "a/b/c"));
    assert!(glob_match("**/**", "a/b/c"));
    assert!(glob_match("*/**", "a/b/c"));
    assert!(glob_match("a/**/j/**/z/*.md", "a/b/c/d/e/j/n/p/o/z/c.md"));
    assert!(glob_match("a/**/z/*.md", "a/b/c/d/e/z/c.md"));
    assert!(glob_match("a/**/c/*.md", "a/bb.bb/aa/b.b/aa/c/xyz.md"));
    assert!(glob_match("a/**/c/*.md", "a/bb.bb/aa/bb/aa/c/xyz.md"));
    assert!(!glob_match("a/**/j/**/z/*.md", "a/b/c/j/e/z/c.txt"));
    assert!(!glob_match("a/b/**/c{d,e}/**/xyz.md", "a/b/c/xyz.md"));
    assert!(!glob_match("a/b/**/c{d,e}/**/xyz.md", "a/b/d/xyz.md"));
    assert!(!glob_match("a/**/", "a/b"));
    // assert!(!glob_match("**/*", "a/b/.js/c.txt"));
    assert!(!glob_match("a/**/", "a/b/c/d"));
    assert!(!glob_match("a/**/", "a/bb"));
    assert!(!glob_match("a/**/", "a/cb"));
    assert!(glob_match("/**", "/a/b"));
    assert!(glob_match("**/*", "a.b"));
    assert!(glob_match("**/*", "a.js"));
    assert!(glob_match("**/*.js", "a.js"));
    // assert!(glob_match("a/**/", "a/"));
    assert!(glob_match("**/*.js", "a/a.js"));
    assert!(glob_match("**/*.js", "a/a/b.js"));
    assert!(glob_match("a/**/b", "a/b"));
    assert!(glob_match("a/**b", "a/b"));
    assert!(glob_match("**/*.md", "a/b.md"));
    assert!(glob_match("**/*", "a/b/c.js"));
    assert!(glob_match("**/*", "a/b/c.txt"));
    assert!(glob_match("a/**/", "a/b/c/d/"));
    assert!(glob_match("**/*", "a/b/c/d/a.js"));
    assert!(glob_match("a/b/**/*.js", "a/b/c/z.js"));
    assert!(glob_match("a/b/**/*.js", "a/b/z.js"));
    assert!(glob_match("**/*", "ab"));
    assert!(glob_match("**/*", "ab/c"));
    assert!(glob_match("**/*", "ab/c/d"));
    assert!(glob_match("**/*", "abc.js"));

    assert!(!glob_match("**/", "a"));
    assert!(!glob_match("**/a/*", "a"));
    assert!(!glob_match("**/a/*/*", "a"));
    assert!(!glob_match("*/a/**", "a"));
    assert!(!glob_match("a/**/*", "a"));
    assert!(!glob_match("a/**/**/*", "a"));
    assert!(!glob_match("**/", "a/b"));
    assert!(!glob_match("**/b/*", "a/b"));
    assert!(!glob_match("**/b/*/*", "a/b"));
    assert!(!glob_match("b/**", "a/b"));
    assert!(!glob_match("**/", "a/b/c"));
    assert!(!glob_match("**/**/b", "a/b/c"));
    assert!(!glob_match("**/b", "a/b/c"));
    assert!(!glob_match("**/b/*/*", "a/b/c"));
    assert!(!glob_match("b/**", "a/b/c"));
    assert!(!glob_match("**/", "a/b/c/d"));
    assert!(!glob_match("**/d/*", "a/b/c/d"));
    assert!(!glob_match("b/**", "a/b/c/d"));
    assert!(glob_match("**", "a"));
    assert!(glob_match("**/**", "a"));
    assert!(glob_match("**/**/*", "a"));
    assert!(glob_match("**/**/a", "a"));
    assert!(glob_match("**/a", "a"));
    // assert!(glob_match("**/a/**", "a"));
    // assert!(glob_match("a/**", "a"));
    assert!(glob_match("**", "a/b"));
    assert!(glob_match("**/**", "a/b"));
    assert!(glob_match("**/**/*", "a/b"));
    assert!(glob_match("**/**/b", "a/b"));
    assert!(glob_match("**/b", "a/b"));
    // assert!(glob_match("**/b/**", "a/b"));
    // assert!(glob_match("*/b/**", "a/b"));
    assert!(glob_match("a/**", "a/b"));
    assert!(glob_match("a/**/*", "a/b"));
    assert!(glob_match("a/**/**/*", "a/b"));
    assert!(glob_match("**", "a/b/c"));
    assert!(glob_match("**/**", "a/b/c"));
    assert!(glob_match("**/**/*", "a/b/c"));
    assert!(glob_match("**/b/*", "a/b/c"));
    assert!(glob_match("**/b/**", "a/b/c"));
    assert!(glob_match("*/b/**", "a/b/c"));
    assert!(glob_match("a/**", "a/b/c"));
    assert!(glob_match("a/**/*", "a/b/c"));
    assert!(glob_match("a/**/**/*", "a/b/c"));
    assert!(glob_match("**", "a/b/c/d"));
    assert!(glob_match("**/**", "a/b/c/d"));
    assert!(glob_match("**/**/*", "a/b/c/d"));
    assert!(glob_match("**/**/d", "a/b/c/d"));
    assert!(glob_match("**/b/**", "a/b/c/d"));
    assert!(glob_match("**/b/*/*", "a/b/c/d"));
    assert!(glob_match("**/d", "a/b/c/d"));
    assert!(glob_match("*/b/**", "a/b/c/d"));
    assert!(glob_match("a/**", "a/b/c/d"));
    assert!(glob_match("a/**/*", "a/b/c/d"));
    assert!(glob_match("a/**/**/*", "a/b/c/d"));
  }

  #[test]
  fn utf8() {
    assert!(glob_match("フ*/**/*", "フォルダ/aaa.js"));
    assert!(glob_match("フォ*/**/*", "フォルダ/aaa.js"));
    assert!(glob_match("フォル*/**/*", "フォルダ/aaa.js"));
    assert!(glob_match("フ*ル*/**/*", "フォルダ/aaa.js"));
    assert!(glob_match("フォルダ/**/*", "フォルダ/aaa.js"));
  }

  #[test]
  fn negation() {
    assert!(!glob_match("!*", "abc"));
    assert!(!glob_match("!abc", "abc"));
    assert!(!glob_match("*!.md", "bar.md"));
    assert!(!glob_match("foo!.md", "bar.md"));
    assert!(!glob_match("\\!*!*.md", "foo!.md"));
    assert!(!glob_match("\\!*!*.md", "foo!bar.md"));
    assert!(glob_match("*!*.md", "!foo!.md"));
    assert!(glob_match("\\!*!*.md", "!foo!.md"));
    assert!(glob_match("!*foo", "abc"));
    assert!(glob_match("!foo*", "abc"));
    assert!(glob_match("!xyz", "abc"));
    assert!(glob_match("*!*.*", "ba!r.js"));
    assert!(glob_match("*.md", "bar.md"));
    assert!(glob_match("*!*.*", "foo!.md"));
    assert!(glob_match("*!*.md", "foo!.md"));
    assert!(glob_match("*!.md", "foo!.md"));
    assert!(glob_match("*.md", "foo!.md"));
    assert!(glob_match("foo!.md", "foo!.md"));
    assert!(glob_match("*!*.md", "foo!bar.md"));
    assert!(glob_match("*b*.md", "foobar.md"));

    assert!(!glob_match("a!!b", "a"));
    assert!(!glob_match("a!!b", "aa"));
    assert!(!glob_match("a!!b", "a/b"));
    assert!(!glob_match("a!!b", "a!b"));
    assert!(glob_match("a!!b", "a!!b"));
    assert!(!glob_match("a!!b", "a/!!/b"));

    assert!(!glob_match("!a/b", "a/b"));
    assert!(glob_match("!a/b", "a"));
    assert!(glob_match("!a/b", "a.b"));
    assert!(glob_match("!a/b", "a/a"));
    assert!(glob_match("!a/b", "a/c"));
    assert!(glob_match("!a/b", "b/a"));
    assert!(glob_match("!a/b", "b/b"));
    assert!(glob_match("!a/b", "b/c"));

    assert!(!glob_match("!abc", "abc"));
    assert!(glob_match("!!abc", "abc"));
    assert!(!glob_match("!!!abc", "abc"));
    assert!(glob_match("!!!!abc", "abc"));
    assert!(!glob_match("!!!!!abc", "abc"));
    assert!(glob_match("!!!!!!abc", "abc"));
    assert!(!glob_match("!!!!!!!abc", "abc"));
    assert!(glob_match("!!!!!!!!abc", "abc"));

    // assert!(!glob_match("!(*/*)", "a/a"));
    // assert!(!glob_match("!(*/*)", "a/b"));
    // assert!(!glob_match("!(*/*)", "a/c"));
    // assert!(!glob_match("!(*/*)", "b/a"));
    // assert!(!glob_match("!(*/*)", "b/b"));
    // assert!(!glob_match("!(*/*)", "b/c"));
    // assert!(!glob_match("!(*/b)", "a/b"));
    // assert!(!glob_match("!(*/b)", "b/b"));
    // assert!(!glob_match("!(a/b)", "a/b"));
    assert!(!glob_match("!*", "a"));
    assert!(!glob_match("!*", "a.b"));
    assert!(!glob_match("!*/*", "a/a"));
    assert!(!glob_match("!*/*", "a/b"));
    assert!(!glob_match("!*/*", "a/c"));
    assert!(!glob_match("!*/*", "b/a"));
    assert!(!glob_match("!*/*", "b/b"));
    assert!(!glob_match("!*/*", "b/c"));
    assert!(!glob_match("!*/b", "a/b"));
    assert!(!glob_match("!*/b", "b/b"));
    assert!(!glob_match("!*/c", "a/c"));
    assert!(!glob_match("!*/c", "a/c"));
    assert!(!glob_match("!*/c", "b/c"));
    assert!(!glob_match("!*/c", "b/c"));
    assert!(!glob_match("!*a*", "bar"));
    assert!(!glob_match("!*a*", "fab"));
    // assert!(!glob_match("!a/(*)", "a/a"));
    // assert!(!glob_match("!a/(*)", "a/b"));
    // assert!(!glob_match("!a/(*)", "a/c"));
    // assert!(!glob_match("!a/(b)", "a/b"));
    assert!(!glob_match("!a/*", "a/a"));
    assert!(!glob_match("!a/*", "a/b"));
    assert!(!glob_match("!a/*", "a/c"));
    assert!(!glob_match("!f*b", "fab"));
    // assert!(glob_match("!(*/*)", "a"));
    // assert!(glob_match("!(*/*)", "a.b"));
    // assert!(glob_match("!(*/b)", "a"));
    // assert!(glob_match("!(*/b)", "a.b"));
    // assert!(glob_match("!(*/b)", "a/a"));
    // assert!(glob_match("!(*/b)", "a/c"));
    // assert!(glob_match("!(*/b)", "b/a"));
    // assert!(glob_match("!(*/b)", "b/c"));
    // assert!(glob_match("!(a/b)", "a"));
    // assert!(glob_match("!(a/b)", "a.b"));
    // assert!(glob_match("!(a/b)", "a/a"));
    // assert!(glob_match("!(a/b)", "a/c"));
    // assert!(glob_match("!(a/b)", "b/a"));
    // assert!(glob_match("!(a/b)", "b/b"));
    // assert!(glob_match("!(a/b)", "b/c"));
    assert!(glob_match("!*", "a/a"));
    assert!(glob_match("!*", "a/b"));
    assert!(glob_match("!*", "a/c"));
    assert!(glob_match("!*", "b/a"));
    assert!(glob_match("!*", "b/b"));
    assert!(glob_match("!*", "b/c"));
    assert!(glob_match("!*/*", "a"));
    assert!(glob_match("!*/*", "a.b"));
    assert!(glob_match("!*/b", "a"));
    assert!(glob_match("!*/b", "a.b"));
    assert!(glob_match("!*/b", "a/a"));
    assert!(glob_match("!*/b", "a/c"));
    assert!(glob_match("!*/b", "b/a"));
    assert!(glob_match("!*/b", "b/c"));
    assert!(glob_match("!*/c", "a"));
    assert!(glob_match("!*/c", "a.b"));
    assert!(glob_match("!*/c", "a/a"));
    assert!(glob_match("!*/c", "a/b"));
    assert!(glob_match("!*/c", "b/a"));
    assert!(glob_match("!*/c", "b/b"));
    assert!(glob_match("!*a*", "foo"));
    // assert!(glob_match("!a/(*)", "a"));
    // assert!(glob_match("!a/(*)", "a.b"));
    // assert!(glob_match("!a/(*)", "b/a"));
    // assert!(glob_match("!a/(*)", "b/b"));
    // assert!(glob_match("!a/(*)", "b/c"));
    // assert!(glob_match("!a/(b)", "a"));
    // assert!(glob_match("!a/(b)", "a.b"));
    // assert!(glob_match("!a/(b)", "a/a"));
    // assert!(glob_match("!a/(b)", "a/c"));
    // assert!(glob_match("!a/(b)", "b/a"));
    // assert!(glob_match("!a/(b)", "b/b"));
    // assert!(glob_match("!a/(b)", "b/c"));
    assert!(glob_match("!a/*", "a"));
    assert!(glob_match("!a/*", "a.b"));
    assert!(glob_match("!a/*", "b/a"));
    assert!(glob_match("!a/*", "b/b"));
    assert!(glob_match("!a/*", "b/c"));
    assert!(glob_match("!f*b", "bar"));
    assert!(glob_match("!f*b", "foo"));

    assert!(!glob_match("!.md", ".md"));
    assert!(glob_match("!**/*.md", "a.js"));
    // assert!(!glob_match("!**/*.md", "b.md"));
    assert!(glob_match("!**/*.md", "c.txt"));
    assert!(glob_match("!*.md", "a.js"));
    assert!(!glob_match("!*.md", "b.md"));
    assert!(glob_match("!*.md", "c.txt"));
    assert!(!glob_match("!*.md", "abc.md"));
    assert!(glob_match("!*.md", "abc.txt"));
    assert!(!glob_match("!*.md", "foo.md"));
    assert!(glob_match("!.md", "foo.md"));

    assert!(glob_match("!*.md", "a.js"));
    assert!(glob_match("!*.md", "b.txt"));
    assert!(!glob_match("!*.md", "c.md"));
    assert!(!glob_match("!a/*/a.js", "a/a/a.js"));
    assert!(!glob_match("!a/*/a.js", "a/b/a.js"));
    assert!(!glob_match("!a/*/a.js", "a/c/a.js"));
    assert!(!glob_match("!a/*/*/a.js", "a/a/a/a.js"));
    assert!(glob_match("!a/*/*/a.js", "b/a/b/a.js"));
    assert!(glob_match("!a/*/*/a.js", "c/a/c/a.js"));
    assert!(!glob_match("!a/a*.txt", "a/a.txt"));
    assert!(glob_match("!a/a*.txt", "a/b.txt"));
    assert!(glob_match("!a/a*.txt", "a/c.txt"));
    assert!(!glob_match("!a.a*.txt", "a.a.txt"));
    assert!(glob_match("!a.a*.txt", "a.b.txt"));
    assert!(glob_match("!a.a*.txt", "a.c.txt"));
    assert!(!glob_match("!a/*.txt", "a/a.txt"));
    assert!(!glob_match("!a/*.txt", "a/b.txt"));
    assert!(!glob_match("!a/*.txt", "a/c.txt"));

    assert!(glob_match("!*.md", "a.js"));
    assert!(glob_match("!*.md", "b.txt"));
    assert!(!glob_match("!*.md", "c.md"));
    // assert!(!glob_match("!**/a.js", "a/a/a.js"));
    // assert!(!glob_match("!**/a.js", "a/b/a.js"));
    // assert!(!glob_match("!**/a.js", "a/c/a.js"));
    assert!(glob_match("!**/a.js", "a/a/b.js"));
    assert!(!glob_match("!a/**/a.js", "a/a/a/a.js"));
    assert!(glob_match("!a/**/a.js", "b/a/b/a.js"));
    assert!(glob_match("!a/**/a.js", "c/a/c/a.js"));
    assert!(glob_match("!**/*.md", "a/b.js"));
    assert!(glob_match("!**/*.md", "a.js"));
    assert!(!glob_match("!**/*.md", "a/b.md"));
    // assert!(!glob_match("!**/*.md", "a.md"));
    assert!(!glob_match("**/*.md", "a/b.js"));
    assert!(!glob_match("**/*.md", "a.js"));
    assert!(glob_match("**/*.md", "a/b.md"));
    assert!(glob_match("**/*.md", "a.md"));
    assert!(glob_match("!**/*.md", "a/b.js"));
    assert!(glob_match("!**/*.md", "a.js"));
    assert!(!glob_match("!**/*.md", "a/b.md"));
    // assert!(!glob_match("!**/*.md", "a.md"));
    assert!(glob_match("!*.md", "a/b.js"));
    assert!(glob_match("!*.md", "a.js"));
    assert!(glob_match("!*.md", "a/b.md"));
    assert!(!glob_match("!*.md", "a.md"));
    assert!(glob_match("!**/*.md", "a.js"));
    // assert!(!glob_match("!**/*.md", "b.md"));
    assert!(glob_match("!**/*.md", "c.txt"));
  }

  #[test]
  fn question_mark() {
    assert!(glob_match("?", "a"));
    assert!(!glob_match("?", "aa"));
    assert!(!glob_match("?", "ab"));
    assert!(!glob_match("?", "aaa"));
    assert!(!glob_match("?", "abcdefg"));

    assert!(!glob_match("??", "a"));
    assert!(glob_match("??", "aa"));
    assert!(glob_match("??", "ab"));
    assert!(!glob_match("??", "aaa"));
    assert!(!glob_match("??", "abcdefg"));

    assert!(!glob_match("???", "a"));
    assert!(!glob_match("???", "aa"));
    assert!(!glob_match("???", "ab"));
    assert!(glob_match("???", "aaa"));
    assert!(!glob_match("???", "abcdefg"));

    assert!(!glob_match("a?c", "aaa"));
    assert!(glob_match("a?c", "aac"));
    assert!(glob_match("a?c", "abc"));
    assert!(!glob_match("ab?", "a"));
    assert!(!glob_match("ab?", "aa"));
    assert!(!glob_match("ab?", "ab"));
    assert!(!glob_match("ab?", "ac"));
    assert!(!glob_match("ab?", "abcd"));
    assert!(!glob_match("ab?", "abbb"));
    assert!(glob_match("a?b", "acb"));

    assert!(!glob_match("a/?/c/?/e.md", "a/bb/c/dd/e.md"));
    assert!(glob_match("a/??/c/??/e.md", "a/bb/c/dd/e.md"));
    assert!(!glob_match("a/??/c.md", "a/bbb/c.md"));
    assert!(glob_match("a/?/c.md", "a/b/c.md"));
    assert!(glob_match("a/?/c/?/e.md", "a/b/c/d/e.md"));
    assert!(!glob_match("a/?/c/???/e.md", "a/b/c/d/e.md"));
    assert!(glob_match("a/?/c/???/e.md", "a/b/c/zzz/e.md"));
    assert!(!glob_match("a/?/c.md", "a/bb/c.md"));
    assert!(glob_match("a/??/c.md", "a/bb/c.md"));
    assert!(glob_match("a/???/c.md", "a/bbb/c.md"));
    assert!(glob_match("a/????/c.md", "a/bbbb/c.md"));
  }

  #[test]
  fn braces() {
    assert!(glob_match("{a,b,c}", "a"));
    assert!(glob_match("{a,b,c}", "b"));
    assert!(glob_match("{a,b,c}", "c"));
    assert!(!glob_match("{a,b,c}", "aa"));
    assert!(!glob_match("{a,b,c}", "bb"));
    assert!(!glob_match("{a,b,c}", "cc"));

    assert!(glob_match("a/{a,b}", "a/a"));
    assert!(glob_match("a/{a,b}", "a/b"));
    assert!(!glob_match("a/{a,b}", "a/c"));
    assert!(!glob_match("a/{a,b}", "b/b"));
    assert!(!glob_match("a/{a,b,c}", "b/b"));
    assert!(glob_match("a/{a,b,c}", "a/c"));
    assert!(glob_match("a{b,bc}.txt", "abc.txt"));

    assert!(glob_match("foo[{a,b}]baz", "foo{baz"));

    assert!(!glob_match("a{,b}.txt", "abc.txt"));
    assert!(!glob_match("a{a,b,}.txt", "abc.txt"));
    assert!(!glob_match("a{b,}.txt", "abc.txt"));
    assert!(glob_match("a{,b}.txt", "a.txt"));
    assert!(glob_match("a{b,}.txt", "a.txt"));
    assert!(glob_match("a{a,b,}.txt", "aa.txt"));
    assert!(glob_match("a{a,b,}.txt", "aa.txt"));
    assert!(glob_match("a{,b}.txt", "ab.txt"));
    assert!(glob_match("a{b,}.txt", "ab.txt"));

    // assert!(glob_match("{a/,}a/**", "a"));
    assert!(glob_match("a{a,b/}*.txt", "aa.txt"));
    assert!(glob_match("a{a,b/}*.txt", "ab/.txt"));
    assert!(glob_match("a{a,b/}*.txt", "ab/a.txt"));
    // assert!(glob_match("{a/,}a/**", "a/"));
    assert!(glob_match("{a/,}a/**", "a/a/"));
    // assert!(glob_match("{a/,}a/**", "a/a"));
    assert!(glob_match("{a/,}a/**", "a/a/a"));
    assert!(glob_match("{a/,}a/**", "a/a/"));
    assert!(glob_match("{a/,}a/**", "a/a/a/"));
    assert!(glob_match("{a/,}b/**", "a/b/a/"));
    assert!(glob_match("{a/,}b/**", "b/a/"));
    assert!(glob_match("a{,/}*.txt", "a.txt"));
    assert!(glob_match("a{,/}*.txt", "ab.txt"));
    assert!(glob_match("a{,/}*.txt", "a/b.txt"));
    assert!(glob_match("a{,/}*.txt", "a/ab.txt"));

    assert!(glob_match("a{,.*{foo,db},\\(bar\\)}.txt", "a.txt"));
    assert!(!glob_match("a{,.*{foo,db},\\(bar\\)}.txt", "adb.txt"));
    assert!(glob_match("a{,.*{foo,db},\\(bar\\)}.txt", "a.db.txt"));

    assert!(glob_match("a{,*.{foo,db},\\(bar\\)}.txt", "a.txt"));
    assert!(!glob_match("a{,*.{foo,db},\\(bar\\)}.txt", "adb.txt"));
    assert!(glob_match("a{,*.{foo,db},\\(bar\\)}.txt", "a.db.txt"));

    // assert!(glob_match("a{,.*{foo,db},\\(bar\\)}", "a"));
    assert!(!glob_match("a{,.*{foo,db},\\(bar\\)}", "adb"));
    assert!(glob_match("a{,.*{foo,db},\\(bar\\)}", "a.db"));

    // assert!(glob_match("a{,*.{foo,db},\\(bar\\)}", "a"));
    assert!(!glob_match("a{,*.{foo,db},\\(bar\\)}", "adb"));
    assert!(glob_match("a{,*.{foo,db},\\(bar\\)}", "a.db"));

    assert!(!glob_match("{,.*{foo,db},\\(bar\\)}", "a"));
    assert!(!glob_match("{,.*{foo,db},\\(bar\\)}", "adb"));
    assert!(!glob_match("{,.*{foo,db},\\(bar\\)}", "a.db"));
    assert!(glob_match("{,.*{foo,db},\\(bar\\)}", ".db"));

    assert!(!glob_match("{,*.{foo,db},\\(bar\\)}", "a"));
    assert!(glob_match("{*,*.{foo,db},\\(bar\\)}", "a"));
    assert!(!glob_match("{,*.{foo,db},\\(bar\\)}", "adb"));
    assert!(glob_match("{,*.{foo,db},\\(bar\\)}", "a.db"));

    assert!(!glob_match("a/b/**/c{d,e}/**/xyz.md", "a/b/c/xyz.md"));
    assert!(!glob_match("a/b/**/c{d,e}/**/xyz.md", "a/b/d/xyz.md"));
    assert!(glob_match("a/b/**/c{d,e}/**/xyz.md", "a/b/cd/xyz.md"));
    assert!(glob_match("a/b/**/{c,d,e}/**/xyz.md", "a/b/c/xyz.md"));
    assert!(glob_match("a/b/**/{c,d,e}/**/xyz.md", "a/b/d/xyz.md"));
    assert!(glob_match("a/b/**/{c,d,e}/**/xyz.md", "a/b/e/xyz.md"));

    assert!(glob_match("*{a,b}*", "xax"));
    assert!(glob_match("*{a,b}*", "xxax"));
    assert!(glob_match("*{a,b}*", "xbx"));

    assert!(glob_match("*{*a,b}", "xba"));
    assert!(glob_match("*{*a,b}", "xb"));

    assert!(!glob_match("*??", "a"));
    assert!(!glob_match("*???", "aa"));
    assert!(glob_match("*???", "aaa"));
    assert!(!glob_match("*****??", "a"));
    assert!(!glob_match("*****???", "aa"));
    assert!(glob_match("*****???", "aaa"));

    assert!(!glob_match("a*?c", "aaa"));
    assert!(glob_match("a*?c", "aac"));
    assert!(glob_match("a*?c", "abc"));

    assert!(glob_match("a**?c", "abc"));
    assert!(!glob_match("a**?c", "abb"));
    assert!(glob_match("a**?c", "acc"));
    assert!(glob_match("a*****?c", "abc"));

    assert!(glob_match("*****?", "a"));
    assert!(glob_match("*****?", "aa"));
    assert!(glob_match("*****?", "abc"));
    assert!(glob_match("*****?", "zzz"));
    assert!(glob_match("*****?", "bbb"));
    assert!(glob_match("*****?", "aaaa"));

    assert!(!glob_match("*****??", "a"));
    assert!(glob_match("*****??", "aa"));
    assert!(glob_match("*****??", "abc"));
    assert!(glob_match("*****??", "zzz"));
    assert!(glob_match("*****??", "bbb"));
    assert!(glob_match("*****??", "aaaa"));

    assert!(!glob_match("?*****??", "a"));
    assert!(!glob_match("?*****??", "aa"));
    assert!(glob_match("?*****??", "abc"));
    assert!(glob_match("?*****??", "zzz"));
    assert!(glob_match("?*****??", "bbb"));
    assert!(glob_match("?*****??", "aaaa"));

    assert!(glob_match("?*****?c", "abc"));
    assert!(!glob_match("?*****?c", "abb"));
    assert!(!glob_match("?*****?c", "zzz"));

    assert!(glob_match("?***?****c", "abc"));
    assert!(!glob_match("?***?****c", "bbb"));
    assert!(!glob_match("?***?****c", "zzz"));

    assert!(glob_match("?***?****?", "abc"));
    assert!(glob_match("?***?****?", "bbb"));
    assert!(glob_match("?***?****?", "zzz"));

    assert!(glob_match("?***?****", "abc"));
    assert!(glob_match("*******c", "abc"));
    assert!(glob_match("*******?", "abc"));
    assert!(glob_match("a*cd**?**??k", "abcdecdhjk"));
    assert!(glob_match("a**?**cd**?**??k", "abcdecdhjk"));
    assert!(glob_match("a**?**cd**?**??k***", "abcdecdhjk"));
    assert!(glob_match("a**?**cd**?**??***k", "abcdecdhjk"));
    assert!(glob_match("a**?**cd**?**??***k**", "abcdecdhjk"));
    assert!(glob_match("a****c**?**??*****", "abcdecdhjk"));

    assert!(!glob_match("a/?/c/?/*/e.md", "a/b/c/d/e.md"));
    assert!(glob_match("a/?/c/?/*/e.md", "a/b/c/d/e/e.md"));
    assert!(glob_match("a/?/c/?/*/e.md", "a/b/c/d/efghijk/e.md"));
    assert!(glob_match("a/?/**/e.md", "a/b/c/d/efghijk/e.md"));
    assert!(!glob_match("a/?/e.md", "a/bb/e.md"));
    assert!(glob_match("a/??/e.md", "a/bb/e.md"));
    assert!(!glob_match("a/?/**/e.md", "a/bb/e.md"));
    assert!(glob_match("a/?/**/e.md", "a/b/ccc/e.md"));
    assert!(glob_match("a/*/?/**/e.md", "a/b/c/d/efghijk/e.md"));
    assert!(glob_match("a/*/?/**/e.md", "a/b/c/d/efgh.ijk/e.md"));
    assert!(glob_match("a/*/?/**/e.md", "a/b.bb/c/d/efgh.ijk/e.md"));
    assert!(glob_match("a/*/?/**/e.md", "a/bbb/c/d/efgh.ijk/e.md"));

    assert!(glob_match("a/*/ab??.md", "a/bbb/abcd.md"));
    assert!(glob_match("a/bbb/ab??.md", "a/bbb/abcd.md"));
    assert!(glob_match("a/bbb/ab???md", "a/bbb/abcd.md"));
  }

  #[test]
  fn captures() {
    fn test_captures<'a>(glob: &str, path: &'a str) -> Option<Vec<&'a str>> {
      glob_match_with_captures(glob, path)
        .map(|v| v.into_iter().map(|capture| &path[capture]).collect())
    }

    assert_eq!(test_captures("a/b", "a/b"), Some(vec![]));
    assert_eq!(test_captures("a/*/c", "a/bx/c"), Some(vec!["bx"]));
    assert_eq!(test_captures("a/*/c", "a/test/c"), Some(vec!["test"]));
    assert_eq!(
      test_captures("a/*/c/*/e", "a/b/c/d/e"),
      Some(vec!["b", "d"])
    );
    assert_eq!(
      test_captures("a/*/c/*/e", "a/b/c/d/e"),
      Some(vec!["b", "d"])
    );
    assert_eq!(test_captures("a/{b,x}/c", "a/b/c"), Some(vec!["b"]));
    assert_eq!(test_captures("a/{b,x}/c", "a/x/c"), Some(vec!["x"]));
    assert_eq!(test_captures("a/?/c", "a/b/c"), Some(vec!["b"]));
    assert_eq!(test_captures("a/*?x/c", "a/yybx/c"), Some(vec!["yy", "b"]));
    assert_eq!(
      test_captures("a/*[a-z]x/c", "a/yybx/c"),
      Some(vec!["yy", "b"])
    );
    assert_eq!(
      test_captures("a/{b*c,c}y", "a/bdcy"),
      Some(vec!["bdc", "d"])
    );
    assert_eq!(test_captures("a/{b*,c}y", "a/bdy"), Some(vec!["bd", "d"]));
    assert_eq!(test_captures("a/{b*c,c}", "a/bdc"), Some(vec!["bdc", "d"]));
    assert_eq!(test_captures("a/{b*,c}", "a/bd"), Some(vec!["bd", "d"]));
    assert_eq!(test_captures("a/{b*,c}", "a/c"), Some(vec!["c", ""]));
    assert_eq!(
      test_captures("a/{b{c,d},c}y", "a/bdy"),
      Some(vec!["bd", "d"])
    );
    assert_eq!(
      test_captures("a/{b*,c*}y", "a/bdy"),
      Some(vec!["bd", "d", ""])
    );
    assert_eq!(
      test_captures("a/{b*,c*}y", "a/cdy"),
      Some(vec!["cd", "", "d"])
    );
    assert_eq!(test_captures("a/{b,c}", "a/b"), Some(vec!["b"]));
    assert_eq!(test_captures("a/{b,c}", "a/c"), Some(vec!["c"]));
    assert_eq!(test_captures("a/{b,c[}]*}", "a/b"), Some(vec!["b", "", ""]));
    assert_eq!(
      test_captures("a/{b,c[}]*}", "a/c}xx"),
      Some(vec!["c}xx", "}", "xx"])
    );

    // assert\.deepEqual\(([!])?capture\('(.*?)', ['"](.*?)['"]\), (.*)?\);
    // assert_eq!(test_captures("$2", "$3"), Some(vec!$4));

    assert_eq!(test_captures("test/*", "test/foo"), Some(vec!["foo"]));
    assert_eq!(
      test_captures("test/*/bar", "test/foo/bar"),
      Some(vec!["foo"])
    );
    assert_eq!(
      test_captures("test/*/bar/*", "test/foo/bar/baz"),
      Some(vec!["foo", "baz"])
    );
    assert_eq!(test_captures("test/*.js", "test/foo.js"), Some(vec!["foo"]));
    assert_eq!(
      test_captures("test/*-controller.js", "test/foo-controller.js"),
      Some(vec!["foo"])
    );

    assert_eq!(
      test_captures("test/**/*.js", "test/a.js"),
      Some(vec!["", "a"])
    );
    assert_eq!(
      test_captures("test/**/*.js", "test/dir/a.js"),
      Some(vec!["dir", "a"])
    );
    assert_eq!(
      test_captures("test/**/*.js", "test/dir/test/a.js"),
      Some(vec!["dir/test", "a"])
    );
    assert_eq!(
      test_captures("**/*.js", "test/dir/a.js"),
      Some(vec!["test/dir", "a"])
    );
    assert_eq!(
      test_captures("**/**/**/**/a", "foo/bar/baz/a"),
      Some(vec!["foo/bar/baz"])
    );
    assert_eq!(
      test_captures("a/{b/**/y,c/**/d}", "a/b/y"),
      Some(vec!["b/y", "", ""])
    );
    assert_eq!(
      test_captures("a/{b/**/y,c/**/d}", "a/b/x/x/y"),
      Some(vec!["b/x/x/y", "x/x", ""])
    );
    assert_eq!(
      test_captures("a/{b/**/y,c/**/d}", "a/c/x/x/d"),
      Some(vec!["c/x/x/d", "", "x/x"])
    );
    assert_eq!(
      test_captures("a/{b/**/**/y,c/**/**/d}", "a/b/x/x/x/x/x/y"),
      Some(vec!["b/x/x/x/x/x/y", "x/x/x/x/x", ""])
    );
    assert_eq!(
      test_captures("a/{b/**/**/y,c/**/**/d}", "a/c/x/x/x/x/x/d"),
      Some(vec!["c/x/x/x/x/x/d", "", "x/x/x/x/x"])
    );
    assert_eq!(
      test_captures(
        "some/**/{a,b,c}/**/needle.txt",
        "some/path/a/to/the/needle.txt"
      ),
      Some(vec!["path", "a", "to/the"])
    );
  }

  #[test]
  fn fuzz_tests() {
    // https://github.com/devongovett/glob-match/issues/1
    let s = "{*{??*{??**,Uz*zz}w**{*{**a,z***b*[!}w??*azzzzzzzz*!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!z[za,z&zz}w**z*z*}";
    assert!(!glob_match(s, s));
    let s = "**** *{*{??*{??***\u{5} *{*{??*{??***\u{5},\0U\0}]*****\u{1},\0***\0,\0\0}w****,\0U\0}]*****\u{1},\0***\0,\0\0}w*****\u{1}***{}*.*\0\0*\0";
    assert!(!glob_match(s, s));
  }
}
