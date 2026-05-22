//! Lightweight visual effect engine for the TUI render loop.
//!
//! Provides fade / paint primitives that operate directly on a ratatui
//! `Buffer`. Each effect advances by wall-clock elapsed time per frame
//! and auto-retires when its duration expires.
//!
//! Design note: tachyonfx 0.25 targets `ratatui-core 0.1` which is
//! type-incompatible with the `ratatui 0.29` this crate uses, so we
//! implement the handful of effects we need in-tree. The public API
//! mirrors tachyonfx's `EffectManager` / `Effect` shape so a future
//! upgrade is straightforward.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

// ---------------------------------------------------------------------------
// Duration
// ---------------------------------------------------------------------------

/// Millisecond duration for effects (avoids pulling `std::time::Duration`
/// into the hot path — we only need integer millis).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FxDuration(u32);

impl FxDuration {
    pub const fn from_millis(ms: u32) -> Self {
        Self(ms)
    }
    pub const fn as_millis(self) -> u32 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Effect primitives
// ---------------------------------------------------------------------------

/// A single visual effect that mutates buffer cells over time.
pub enum Effect {
    /// Linearly interpolate every cell's foreground from `from` toward
    /// the cell's existing fg over `total_ms`.
    FadeFromFg {
        from: Color,
        total_ms: u32,
        elapsed_ms: u32,
    },
    /// Linearly interpolate every cell's foreground from its current
    /// value toward `to` over `total_ms`.
    FadeToFg {
        to: Color,
        total_ms: u32,
        elapsed_ms: u32,
    },
    /// Paint every cell's foreground to `color` for `total_ms`, then
    /// finish.
    PaintFg {
        color: Color,
        total_ms: u32,
        elapsed_ms: u32,
    },
    /// Run effects one after another.
    Sequence {
        steps: Vec<Effect>,
        index: usize,
    },
    /// Run effects simultaneously; done when all children finish.
    Parallel(Vec<Effect>),
}

impl Effect {
    pub fn done(&self) -> bool {
        match self {
            Self::FadeFromFg {
                total_ms,
                elapsed_ms,
                ..
            }
            | Self::FadeToFg {
                total_ms,
                elapsed_ms,
                ..
            }
            | Self::PaintFg {
                total_ms,
                elapsed_ms,
                ..
            } => *elapsed_ms >= *total_ms,
            Self::Sequence { steps, index } => *index >= steps.len(),
            Self::Parallel(children) => children.iter().all(|c| c.done()),
        }
    }

    pub fn process(&mut self, dt: FxDuration, buf: &mut Buffer, area: Rect) {
        match self {
            Self::FadeFromFg {
                from,
                total_ms,
                elapsed_ms,
            } => {
                *elapsed_ms = elapsed_ms.saturating_add(dt.as_millis()).min(*total_ms);
                let t = *elapsed_ms as f64 / *total_ms as f64;
                for y in area.top()..area.bottom() {
                    for x in area.left()..area.right() {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            let target = cell.fg;
                            cell.fg = lerp_color(*from, target, t);
                        }
                    }
                }
            }
            Self::FadeToFg {
                to,
                total_ms,
                elapsed_ms,
            } => {
                *elapsed_ms = elapsed_ms.saturating_add(dt.as_millis()).min(*total_ms);
                let t = *elapsed_ms as f64 / *total_ms as f64;
                for y in area.top()..area.bottom() {
                    for x in area.left()..area.right() {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            let current = cell.fg;
                            cell.fg = lerp_color(current, *to, t);
                        }
                    }
                }
            }
            Self::PaintFg {
                color,
                total_ms,
                elapsed_ms,
            } => {
                *elapsed_ms = elapsed_ms.saturating_add(dt.as_millis()).min(*total_ms);
                for y in area.top()..area.bottom() {
                    for x in area.left()..area.right() {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.fg = *color;
                        }
                    }
                }
            }
            Self::Sequence { steps, index } => {
                if *index < steps.len() {
                    steps[*index].process(dt, buf, area);
                    if steps[*index].done() {
                        *index += 1;
                    }
                }
            }
            Self::Parallel(children) => {
                for child in children.iter_mut() {
                    if !child.done() {
                        child.process(dt, buf, area);
                    }
                }
            }
        }
    }
}

/// Convenience constructors matching the tachyonfx `fx::` namespace.
pub mod fx {
    use super::*;

    pub fn fade_from_fg(from: Color, duration: FxDuration) -> Effect {
        Effect::FadeFromFg {
            from,
            total_ms: duration.as_millis(),
            elapsed_ms: 0,
        }
    }

    pub fn fade_to_fg(to: Color, duration: FxDuration) -> Effect {
        Effect::FadeToFg {
            to,
            total_ms: duration.as_millis(),
            elapsed_ms: 0,
        }
    }

    pub fn paint_fg(color: Color, duration: FxDuration) -> Effect {
        Effect::PaintFg {
            color,
            total_ms: duration.as_millis(),
            elapsed_ms: 0,
        }
    }

    pub fn sequence(steps: Vec<Effect>) -> Effect {
        Effect::Sequence { steps, index: 0 }
    }

    pub fn parallel(children: Vec<Effect>) -> Effect {
        Effect::Parallel(children)
    }
}

// ---------------------------------------------------------------------------
// Color interpolation
// ---------------------------------------------------------------------------

/// Linear interpolation between two `Color::Rgb` values. Falls back to
/// `to` if either input is not an Rgb variant (ANSI colors don't have
/// a meaningful lerp).
fn lerp_color(from: Color, to: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => Color::Rgb(
            lerp_u8(r1, r2, t),
            lerp_u8(g1, g2, t),
            lerp_u8(b1, b2, t),
        ),
        _ => {
            if t >= 0.5 {
                to
            } else {
                from
            }
        }
    }
}

fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * t).round() as u8
}

// ---------------------------------------------------------------------------
// EffectKind + EffectManager
// ---------------------------------------------------------------------------

/// Semantic tag for an in-flight effect. Used by [`EffectManager::clear_kind`]
/// to remove a category without disturbing others.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EffectKind {
    OverlayOpen,
    OverlayClose,
    NewMessage,
    StatusFlash,
    ErrorFlash,
}

/// Thin bookkeeper: owns a `Vec` of `(kind, effect, area)` triples,
/// ticks them each frame, and retires finished entries.
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
    pub fn process_frame(&mut self, buf: &mut Buffer, elapsed: FxDuration) {
        self.items.retain_mut(|(_, e, area)| {
            e.process(elapsed, buf, *area);
            !e.done()
        });
    }
}
