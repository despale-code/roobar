//! Hover cards: the hint text shown when hovering a bar widget or a popover row.
//!
//! egui's own tooltips are drawn inside the window of the widget they belong to, and the bar is
//! only `--bar-height` tall (popovers are sized to their content), so they were cut off. A card
//! is its own small always-on-top window instead: it never takes focus, lets clicks through, sits
//! under the hovered widget and stays on the monitor.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

const TITLE: &str = "roobar hover card";
/// Hover this long before the card appears.
const DELAY: Duration = Duration::from_millis(450);
const MAX_TEXT_W: f32 = 320.0;

thread_local! {
    /// Card requested this frame: (text, widget rect on screen in points).
    static PENDING: RefCell<Option<(String, egui::Rect)>> = const { RefCell::new(None) };
    /// Screen position of the window being drawn, so widget rects can be made absolute.
    static ORIGIN: Cell<egui::Vec2> = const { Cell::new(egui::Vec2::ZERO) };
}

/// Sets where the window whose widgets are drawn next sits on screen (points).
pub fn set_origin(origin: egui::Pos2) {
    ORIGIN.with(|o| o.set(origin.to_vec2()));
}

pub trait HoverCardExt {
    /// Like `on_hover_text`, but shown in a card window that isn't clipped by the bar.
    fn hover_card(self, text: impl Into<String>) -> Self;
}

impl HoverCardExt for egui::Response {
    fn hover_card(self, text: impl Into<String>) -> Self {
        if self.hovered() && !self.dragged() {
            let rect = self.rect.translate(ORIGIN.with(Cell::get));
            PENDING.with(|p| *p.borrow_mut() = Some((text.into(), rect)));
        }
        self
    }
}

#[derive(Default)]
pub struct HoverCards {
    /// The card being hovered: text, widget rect and since when.
    current: Option<(String, egui::Rect, Instant)>,
    shown: bool,
}

impl HoverCards {
    /// Call before drawing the frame's widgets.
    pub fn begin_frame(&mut self) {
        PENDING.with(|p| p.borrow_mut().take());
    }

    /// Shows (or hides) the card requested while drawing this frame. `suppress` hides every
    /// card; cards for widgets inside `suppress_in` (the bar, while a popover hangs from it)
    /// are hidden too.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        screen: egui::Rect,
        bg: egui::Color32,
        border: egui::Color32,
        fg: egui::Color32,
        radius: f32,
        font: egui::FontId,
        suppress: bool,
        suppress_in: Option<egui::Rect>,
    ) {
        let pending = PENDING
            .with(|p| p.borrow_mut().take())
            .filter(|(_, rect)| !suppress && !suppress_in.is_some_and(|r| r.contains(rect.center())));
        self.current = match (pending, self.current.take()) {
            (Some((text, rect)), Some((old_text, old_rect, since))) if text == old_text && rect == old_rect => {
                Some((text, rect, since))
            }
            (Some((text, rect)), _) => Some((text, rect, Instant::now())),
            (None, _) => None,
        };
        let Some((text, anchor, since)) = &self.current else {
            self.shown = false;
            return;
        };
        let waited = since.elapsed();
        if waited < DELAY {
            ctx.request_repaint_after(DELAY - waited);
            return;
        }

        let galley = ctx.fonts(|f| f.layout(text.clone(), font.clone(), fg, MAX_TEXT_W));
        let pad = egui::vec2(10.0, 6.0);
        let size = (galley.size() + pad * 2.0).ceil();
        let x = (anchor.center().x - size.x / 2.0)
            .clamp(screen.left() + 6.0, (screen.right() - size.x - 6.0).max(screen.left()));
        // Under the widget; above it when there's no room below.
        let below = anchor.bottom() + 6.0;
        let y = if below + size.y > screen.bottom() - 6.0 { (anchor.top() - 6.0 - size.y).max(screen.top()) } else { below };

        let builder = egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_resizable(false)
            .with_taskbar(false)
            .with_active(false)
            .with_mouse_passthrough(true)
            .with_position(egui::pos2(x, y))
            .with_inner_size(size);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("roobar-hover-card"), builder, |pctx, _| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(bg).stroke(egui::Stroke::new(1.0_f32, border)).corner_radius(radius.min(10.0)))
                .show(pctx, |ui| {
                    ui.painter().galley(ui.max_rect().min + pad, galley.clone(), fg);
                });
        });
        if !self.shown && crate::prepare_window_no_transitions(TITLE) {
            self.shown = true;
        }
    }
}
