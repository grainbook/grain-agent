//! TUI render-loop animation bookkeeping.
//!
//! Thin wrapper around `tachyonfx 0.25`: the effect primitives (fade,
//! paint, sequence, parallel, etc.) come straight from `tachyonfx::fx`;
//! we only own [`EffectKind`] (semantic tag) + [`EffectManager`] (ticks
//! and retires per-frame).
//!
//! tachyonfx 0.25 targets `ratatui-core ^0.1`, which `ratatui 0.30`
//! re-exports — so the `Buffer` / `Rect` types are shared and no shim
//! is needed. (This module replaced a 200-line in-tree shim that
//! existed only while the workspace was on `ratatui 0.29`; see commit
//! history if you need the rationale.)

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

pub use tachyonfx::Duration;
pub use tachyonfx::Effect;
pub use tachyonfx::fx;

/// Back-compat alias. Old call sites used `FxDuration::from_millis(N)`;
/// the tachyonfx-native name is `Duration` (= `std::time::Duration`).
/// Kept so we don't churn every call site in the same commit that
/// adopts tachyonfx.
pub type FxDuration = Duration;

/// Semantic tag for an in-flight effect. Used by
/// [`EffectManager::clear_kind`] to remove a category without
/// disturbing others.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EffectKind {
    OverlayOpen,
    OverlayClose,
    NewMessage,
    StatusFlash,
    ErrorFlash,
}

/// Owns active effects, ticks them each frame, and retires the ones
/// that report `done()`.
pub struct EffectManager {
    items: Vec<(EffectKind, Effect, Rect)>,
}

impl std::fmt::Debug for EffectManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectManager")
            .field("active", &self.items.len())
            .finish()
    }
}

impl Default for EffectManager {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectManager {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn push(&mut self, kind: EffectKind, effect: Effect, area: Rect) {
        self.items.push((kind, effect, area));
    }

    pub fn clear_kind(&mut self, kind: EffectKind) {
        self.items.retain(|(k, _, _)| *k != kind);
    }

    pub fn is_active(&self) -> bool {
        !self.items.is_empty()
    }

    /// Advance every effect by `elapsed` and drop the ones that report
    /// [`Effect::done`].
    pub fn process_frame(&mut self, buf: &mut Buffer, elapsed: Duration) {
        self.items.retain_mut(|(_, e, area)| {
            let _ = e.process(elapsed, buf, *area);
            !e.done()
        });
    }
}
