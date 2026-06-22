// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::io::Write;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anstream::stream::IsTerminal;
use anstyle::Style;
use parking_lot::Mutex;
use parking_lot::RwLock;

use self::render::ProgressBarSnapshot;
use self::render::format_bar_line;
use self::render::format_spinner_line;
use self::render::spinner_frame;
use crate::config::config;
use crate::progress_bar_internal_print;
use crate::styling::CommonStyles;
use crate::terminal_size::terminal_size;

pub mod clone;
pub mod render;
pub mod sync;

const MIN_TERM_WIDTH: u16 = 10;

static CURRENT_PROGRESS_BAR: RwLock<WeakProgressBar> = RwLock::new(WeakProgressBar::empty());

pub struct ProgressBar {
    data: Arc<Mutex<ProgressBarData>>,
    /// `true` for canonical handles; `false` for temporary handles vended by
    /// `WeakProgressBar::upgrade()`. `Drop` only signals stop when `true`, so
    /// temporary handles don't prematurely stop the render thread. `Clone` is
    /// intentionally NOT derived — cloning would duplicate ownership.
    owner: bool,
}

#[derive(Default)]
pub struct ProgressBarData {
    pub progress: u64,
    pub max_progress: u64,
    pub suspend_token: ProgressBarSuspendToken,
    pub options: ProgressBarOptions,
    pub is_terminal: bool,
    pub terminal_width: u16,
    pub message: String,
    pub is_growing: bool,
    pub frame_tick: u64,
    pub stop: bool,
}

pub struct ProgressBarOptions {
    pub units: Option<String>,
    pub max_len: u16,
    pub fg_color: Style,
    pub bg_color: Style,
}

impl Default for ProgressBarOptions {
    fn default() -> Self {
        Self {
            units: None,
            max_len: 20,
            fg_color: CommonStyles::PROGRESS_FG,
            bg_color: CommonStyles::PROGRESS_BG,
        }
    }
}

#[derive(Default, Clone)]
#[must_use]
pub struct ProgressBarSuspendToken {
    token: Arc<AtomicUsize>,
}

#[derive(Default, Clone)]
struct WeakProgressBar {
    data: Weak<Mutex<ProgressBarData>>,
}

impl ProgressBar {
    pub fn new(max_progress: u64) -> Self {
        Self::new_with_options(max_progress, Default::default())
    }

    /// Temporary handle vended by `WeakProgressBar::upgrade()`. Non-owning:
    /// `Drop` does not signal stop, so dropping the temporary does not stop
    /// the render thread.
    fn new_non_owner(data: Arc<Mutex<ProgressBarData>>) -> Self {
        Self { data, owner: false }
    }

    pub fn new_spinner(message: impl Into<String>) -> Self {
        let pb = Self {
            data: Arc::new(Mutex::new(ProgressBarData {
                progress: 0,
                max_progress: 0,
                suspend_token: Default::default(),
                options: Default::default(),
                is_terminal: std::io::stdout().is_terminal(),
                terminal_width: terminal_width().unwrap_or_default(),
                message: message.into(),
                is_growing: false,
                frame_tick: 0,
                stop: false,
            })),
            owner: true,
        };
        stop_previous_if_live();
        *CURRENT_PROGRESS_BAR.write() = pb.downgrade();
        pb.spawn_render_thread();
        pb
    }

    pub fn new_with_options(max_progress: u64, options: ProgressBarOptions) -> Self {
        let pb = Self {
            data: Arc::new(Mutex::new(ProgressBarData {
                progress: 0,
                max_progress,
                suspend_token: Default::default(),
                options,
                is_terminal: std::io::stdout().is_terminal(),
                terminal_width: terminal_width().unwrap_or_default(),
                message: String::new(),
                is_growing: false,
                frame_tick: 0,
                stop: false,
            })),
            owner: true,
        };
        stop_previous_if_live();
        *CURRENT_PROGRESS_BAR.write() = pb.downgrade();
        pb.spawn_render_thread();
        pb
    }

    /// The thread holds a `Weak` so it exits cleanly once the last strong ref
    /// is dropped, without `Drop` ever having to join.
    fn spawn_render_thread(&self) {
        let weak = Arc::downgrade(&self.data);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));

                let arc = match weak.upgrade() {
                    Some(a) => a,
                    None => return,
                };

                let mut data = arc.lock();
                if data.stop {
                    return;
                }

                data.terminal_width = terminal_width().unwrap_or_default();
                data.frame_tick = data.frame_tick.wrapping_add(1);

                if !data.is_visible() {
                    continue;
                }

                // Hold the lock across the write so the format + write is
                // atomic with respect to concurrent print! calls.
                let max = data.max_progress;
                let frame_tick = data.frame_tick;
                let message = data.message.clone();
                let bar_width = data.terminal_width;

                if max == 0 {
                    let frame = spinner_frame(frame_tick);
                    let line = format_spinner_line(frame, &message);
                    progress_bar_internal_print!("\x1b[2K\r{line}");
                } else {
                    let snap = ProgressBarSnapshot {
                        progress: data.progress,
                        max,
                        is_growing: data.is_growing,
                        message: &message,
                        units: data.options.units.as_deref(),
                        fg_color: data.options.fg_color,
                        bg_color: data.options.bg_color,
                    };
                    let effective_bar_width = data.options.max_len.min(bar_width);
                    let line = format_bar_line(&snap, effective_bar_width);
                    if !line.is_empty() {
                        progress_bar_internal_print!("\x1b[2K\r{line}");
                    }
                }
                let _ = std::io::stdout().flush();
            }
        });
    }

    pub fn set_max_progress(&self, max_progress: u64) {
        self.data.lock().set_max_progress(max_progress);
    }

    pub fn set_progress(&self, progress: u64) {
        self.data.lock().set_progress(progress);
    }

    pub fn set_message(&self, message: impl Into<String>) {
        self.data.lock().message = message.into();
    }

    pub fn set_growing(&self, growing: bool) {
        self.data.lock().is_growing = growing;
    }

    fn suspend(&self) -> ProgressBarSuspendToken {
        self.data.lock().suspend()
    }

    #[must_use]
    fn downgrade(&self) -> WeakProgressBar {
        WeakProgressBar::new(self)
    }
}

impl ProgressBarData {
    fn is_visible(&self) -> bool {
        self.is_terminal
            && self.suspend_token.token.load(Ordering::SeqCst) == 0
            && self.terminal_width >= MIN_TERM_WIDTH
            && !self.stop
    }

    fn suspend(&mut self) -> ProgressBarSuspendToken {
        self.clear();
        self.suspend_token.token.fetch_add(1, Ordering::SeqCst);
        self.suspend_token.clone()
    }

    fn set_progress(&mut self, progress: u64) {
        self.progress = progress;
    }

    fn set_max_progress(&mut self, max_progress: u64) {
        self.max_progress = max_progress;
    }

    fn clear(&self) {
        if self.is_visible() {
            progress_bar_internal_print!("\x1b[2K\r");
            let _ = std::io::stdout().flush();
        }
    }
}

impl Drop for ProgressBar {
    fn drop(&mut self) {
        // Temporary handles (owner=false) must not stop the render thread.
        if !self.owner {
            return;
        }
        let mut data = self.data.lock();
        data.stop = true;
        // Clear the line synchronously so the indicator disappears immediately,
        // without waiting for the render thread to observe `stop` on its next tick.
        if data.is_terminal {
            progress_bar_internal_print!("\x1b[2K\r");
            let _ = std::io::stdout().flush();
        }
        // Thread is detached; Drop does not join.
    }
}

impl Drop for ProgressBarSuspendToken {
    fn drop(&mut self) {
        // The render thread will redraw on its next tick (≤100 ms) once the
        // counter reaches 0.
        self.token.fetch_sub(1, Ordering::SeqCst);
    }
}

impl WeakProgressBar {
    fn new(pb: &ProgressBar) -> Self {
        Self {
            data: Arc::downgrade(&pb.data),
        }
    }

    const fn empty() -> Self {
        Self { data: Weak::new() }
    }

    fn upgrade(&self) -> Option<ProgressBar> {
        self.data.upgrade().map(ProgressBar::new_non_owner)
    }
}

#[must_use]
pub fn suspend_current_progress_bar() -> Option<ProgressBarSuspendToken> {
    CURRENT_PROGRESS_BAR.read().upgrade().map(|pb| pb.suspend())
}

pub fn terminal_width() -> Option<u16> {
    terminal_size().map(|(w, _)| w)
}

pub fn progress_debug() -> bool {
    config().debug
}

/// Only one indicator can be live at a time. Before registering a new one,
/// stop the previous (if any) so its render thread exits on its next tick.
fn stop_previous_if_live() {
    if let Some(prev_data) = CURRENT_PROGRESS_BAR.read().data.upgrade() {
        crate::eprintln!(
            "[Warning] ProgressBar created when previous indicator still live; replacing silently"
        );
        prev_data.lock().stop = true;
    }
}
