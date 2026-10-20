//! Popovers that drop down from bar widgets: a calendar on the clock, volume and output device
//! on the volume widget, the heaviest processes on the stats widgets, and player controls on the
//! media widget.
//!
//! Each is a small borderless always-on-top viewport placed under its widget. It closes on Esc,
//! when it loses focus (a click anywhere else), or when its widget is clicked again.

use crate::hovercard::HoverCardExt;
use crate::procs::{self, ProcRow, ProcSampler};
use crate::status::{Cmd, Status};
use crate::widgets;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopoverKind {
    Calendar,
    Volume,
    Processes,
    Media,
    Network,
    /// The hamburger menu at the right end of the bar.
    Menu,
    About,
}

/// A pick in the hamburger menu, carried out by the bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Files,
    Settings,
    AboutChickenWm,
    Shutdown,
}

impl MenuAction {
    pub const ALL: [MenuAction; 4] =
        [MenuAction::Files, MenuAction::Settings, MenuAction::AboutChickenWm, MenuAction::Shutdown];

    fn label(self) -> &'static str {
        match self {
            MenuAction::Files => "File Explorer",
            MenuAction::Settings => "Settings",
            MenuAction::AboutChickenWm => "About ChickenWM",
            MenuAction::Shutdown => "Shut down…",
        }
    }
}

impl PopoverKind {
    pub fn size(self, status: &Status) -> egui::Vec2 {
        match self {
            PopoverKind::Menu => egui::vec2(200.0, 20.0 + 31.0 * MenuAction::ALL.len() as f32),
            PopoverKind::About => egui::vec2(300.0, 172.0),
            PopoverKind::Calendar => egui::vec2(300.0, 290.0),
            PopoverKind::Volume => egui::vec2(340.0, 128.0 + 30.0 * status.outputs.len() as f32),
            PopoverKind::Processes => egui::vec2(380.0, 330.0),
            PopoverKind::Media => egui::vec2(
                368.0,
                if status.media_players.len() > 1 { 208.0 } else { 176.0 },
            ),
            PopoverKind::Network => {
                let rows = status.wifi_networks.len().clamp(1, 7) as f32;
                egui::vec2(340.0, 136.0 + 30.0 * rows)
            }
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            PopoverKind::Calendar => "roobar calendar",
            PopoverKind::Volume => "roobar volume",
            PopoverKind::Processes => "roobar processes",
            PopoverKind::Media => "roobar media",
            PopoverKind::Network => "roobar network",
            PopoverKind::Menu => "roobar menu",
            PopoverKind::About => "roobar about",
        }
    }
}

/// Samples processes once a second on its own thread while the popover is open.
struct ProcFeed {
    rows: Arc<Mutex<Vec<ProcRow>>>,
    stop: Arc<AtomicBool>,
}

impl ProcFeed {
    fn start(ctx: egui::Context) -> Self {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (out, quit) = (Arc::clone(&rows), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut sampler = ProcSampler::default();
            sampler.sample(); // Baseline for the CPU deltas.
            std::thread::sleep(Duration::from_millis(400));
            while !quit.load(Ordering::Relaxed) {
                *out.lock() = sampler.sample();
                ctx.request_repaint();
                std::thread::sleep(Duration::from_millis(1000));
            }
        });
        Self { rows, stop }
    }
}

impl Drop for ProcFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub struct Popover {
    pub kind: PopoverKind,
    /// The widget it hangs from, in bar coordinates.
    pub anchor: egui::Rect,
    /// Calendar: the month shown.
    month: (i32, u32),
    procs: Option<ProcFeed>,
    focused_once: bool,
    /// Height the content took last frame; the window shrinks to it (`size()` is the maximum).
    content_h: Option<f32>,
    /// The window size last sent to the viewport (builder size changes aren't applied to
    /// immediate viewports, so resizes go out as explicit commands).
    applied_size: Option<egui::Vec2>,
    /// A mouse button was down last frame (to see new clicks outside the popover).
    mouse_down: bool,
    opened: Instant,
    /// Set once dismissed; the popover fades out before closing.
    pub closing: Option<Instant>,
    /// True once DWM transitions have been disabled on the popover HWND.
    shown: bool,
    /// Keyboard-selected row index for list navigation.
    selected_idx: usize,
    /// The cursor has been over the popover (or its widget) since it opened.
    hovered_once: bool,
    /// When the cursor left the popover and its widget (it closes shortly after).
    left_at: Option<Instant>,
}

/// How long the cursor may be away from a popover before it closes.
const HOVER_LEAVE_GRACE: Duration = Duration::from_millis(450);

/// Colours and font the popover draws with (the bar's own).
pub struct PopoverStyle {
    pub bg: egui::Color32,
    pub border: egui::Color32,
    pub fg: egui::Color32,
    pub subtext: egui::Color32,
    pub accent_bg: egui::Color32,
    pub accent_fg: egui::Color32,
    pub radius: f32,
    pub font: egui::FontId,
    /// The font is a Nerd Font (icon glyphs available).
    pub nerd: bool,
    pub anim_secs: f32,
}

#[derive(Default)]
pub struct Outcome {
    pub close: bool,
    pub cmds: Vec<Cmd>,
    pub open_uri: Option<&'static str>,
    pub menu: Option<MenuAction>,
}

impl Popover {
    pub fn new(ctx: &egui::Context, kind: PopoverKind, anchor: egui::Rect) -> Self {
        let now = local_now();
        Self {
            kind,
            anchor,
            month: (i32::from(now.wYear), u32::from(now.wMonth)),
            procs: (kind == PopoverKind::Processes).then(|| ProcFeed::start(ctx.clone())),
            focused_once: false,
            content_h: None,
            applied_size: None,
            // The click that opened it may still be held.
            mouse_down: true,
            opened: Instant::now(),
            closing: None,
            shown: false,
            selected_idx: 0,
            hovered_once: false,
            left_at: None,
        }
    }

    pub fn begin_close(&mut self) {
        if self.closing.is_none() {
            self.closing = Some(Instant::now());
        }
    }

    /// Shows the popover for this frame. `bar` is the bar window's outer rect on screen and
    /// `screen` the monitor, both in points.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        bar: egui::Rect,
        screen: egui::Rect,
        style: &PopoverStyle,
        status: &Status,
    ) -> Outcome {
        let max_size = self.kind.size(status);
        let size = egui::vec2(max_size.x, self.content_h.map_or(max_size.y, |h| h.clamp(48.0, max_size.y)));
        // Centred under the widget, kept on the monitor.
        let x = (bar.left() + self.anchor.center().x - size.x / 2.0)
            .clamp(screen.left() + 6.0, (screen.right() - size.x - 6.0).max(screen.left()));
        let pos = egui::pos2(x, bar.bottom() + 6.0);
        let pop_rect = egui::Rect::from_min_size(pos, size);

        // Hovering off closes it: once the cursor has been over the popover or its widget, being
        // away from both (and the gap between them) for a moment dismisses it. Not while a button
        // is held (dragging the volume slider past the edge).
        let left_by_hover = {
            use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON};
            use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
            let mut pt = windows::Win32::Foundation::POINT::default();
            let cursor = unsafe { GetCursorPos(&mut pt) }.is_ok().then(|| {
                let ppp = ctx.pixels_per_point();
                egui::pos2(pt.x as f32 / ppp, pt.y as f32 / ppp)
            });
            let widget = self.anchor.translate(bar.min.to_vec2());
            let gap = egui::Rect::from_min_max(
                egui::pos2(widget.left().max(pop_rect.left()), widget.bottom() - 4.0),
                egui::pos2(widget.right().min(pop_rect.right()), pop_rect.top() + 4.0),
            );
            let inside = cursor.is_some_and(|p| {
                pop_rect.expand(16.0).contains(p)
                    || widget.expand(6.0).contains(p)
                    || gap.expand2(egui::vec2(16.0, 6.0)).contains(p)
            });
            let held = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
                .iter()
                .any(|k| unsafe { GetAsyncKeyState(i32::from(k.0)) } as u16 & 0x8000 != 0);
            if inside || held {
                self.hovered_once |= inside;
                self.left_at = None;
                false
            } else if self.hovered_once {
                self.left_at.get_or_insert_with(Instant::now).elapsed() >= HOVER_LEAVE_GRACE
            } else {
                false
            }
        };

        // A click anywhere outside the popover (and its widget, which toggles it itself) closes
        // it. Polled, because the popover doesn't always get focus to lose.
        let clicked_outside = {
            use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON};
            use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
            let down = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
                .iter()
                .any(|k| unsafe { GetAsyncKeyState(i32::from(k.0)) } as u16 & 0x8000 != 0);
            let pressed = down && !self.mouse_down;
            self.mouse_down = down;
            let mut pt = windows::Win32::Foundation::POINT::default();
            pressed
                && self.opened.elapsed() > Duration::from_millis(150)
                && unsafe { GetCursorPos(&mut pt) }.is_ok()
                && {
                    let ppp = ctx.pixels_per_point();
                    let p = egui::pos2(pt.x as f32 / ppp, pt.y as f32 / ppp);
                    !pop_rect.contains(p) && !self.anchor.translate(bar.min.to_vec2()).contains(p)
                }
        };

        let anim_secs = style.anim_secs.max(0.05);
        let ease = |t: f32| 1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3);
        let open_t = ease(self.opened.elapsed().as_secs_f32() / anim_secs);
        let close_t = self
            .closing
            .map(|at| at.elapsed().as_secs_f32() / (anim_secs * 0.8))
            .unwrap_or(0.0);
        let fade_out = 1.0 - ease(close_t);
        let appear = (open_t * fade_out).clamp(0.0, 1.0);
        let animating = open_t < 1.0 || self.closing.is_some();

        let builder = egui::ViewportBuilder::default()
            .with_title(self.kind.title())
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_resizable(false)
            .with_taskbar(false)
            .with_active(true)
            .with_position(pos)
            .with_inner_size(size);

        let mut out = Outcome::default();
        let mut request_dismiss = false;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("roobar-popover"), builder, |pctx, _class| {
            crate::hovercard::set_origin(pos);
            if self.applied_size != Some(size) {
                if self.applied_size.is_some() {
                    pctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                }
                self.applied_size = Some(size);
            }
            let eff_style = PopoverStyle {
                bg: style.bg.gamma_multiply(appear),
                border: style.border.gamma_multiply(appear),
                fg: style.fg.gamma_multiply(appear),
                subtext: style.subtext.gamma_multiply(appear),
                accent_bg: style.accent_bg.gamma_multiply(appear),
                accent_fg: style.accent_fg.gamma_multiply(appear),
                radius: style.radius,
                font: style.font.clone(),
                nerd: style.nerd,
                anim_secs: style.anim_secs,
            };

            // The card fills the whole window (nothing left transparent around it, which some
            // setups draw black); the content sits inside it with padding.
            let frame = egui::Frame::NONE
                .fill(egui::Color32::TRANSPARENT)
                .inner_margin(egui::Margin::symmetric(12, 10));
            egui::CentralPanel::default().frame(frame).show(pctx, |ui| {
                let full = pctx.screen_rect();
                let slide_y = (1.0 - appear) * -4.0;
                let card_rect = full.translate(egui::vec2(0.0, slide_y));
                let painter = pctx.layer_painter(egui::LayerId::background());

                // Clean translucent surface (media popover mixes theme accent into the surface)
                let surface_fill = if self.kind == PopoverKind::Media {
                    blend_color(eff_style.bg.gamma_multiply(0.92), eff_style.accent_bg, 0.16)
                } else {
                    eff_style.bg.gamma_multiply(0.94)
                };
                let shadow_color = egui::Color32::from_black_alpha((75.0 * appear) as u8);
                painter.rect(
                    card_rect.expand(2.0),
                    eff_style.radius + 2.0,
                    egui::Color32::TRANSPARENT,
                    egui::Stroke::new(1.0_f32, shadow_color),
                    egui::StrokeKind::Outside,
                );
                painter.rect(
                    card_rect,
                    eff_style.radius,
                    surface_fill,
                    egui::Stroke::new(1.0_f32, eff_style.border),
                    egui::StrokeKind::Inside,
                );

                let faint = egui::Stroke::new(1.0_f32, eff_style.border.gamma_multiply(0.35));
                let v = ui.visuals_mut();
                v.override_text_color = Some(eff_style.fg);
                v.selection.bg_fill = eff_style.accent_bg;
                v.selection.stroke = faint;
                v.widgets.inactive.weak_bg_fill = eff_style.accent_bg.gamma_multiply(0.10);
                v.widgets.inactive.bg_fill = eff_style.accent_bg.gamma_multiply(0.25);
                v.widgets.inactive.bg_stroke = faint;
                v.widgets.hovered.weak_bg_fill = eff_style.accent_bg.gamma_multiply(0.35);
                v.widgets.hovered.bg_fill = eff_style.accent_bg.gamma_multiply(0.45);
                v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, eff_style.border.gamma_multiply(0.6));
                v.widgets.active.weak_bg_fill = eff_style.accent_bg;
                v.widgets.active.bg_fill = eff_style.accent_bg;
                v.widgets.noninteractive.bg_stroke = faint;
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.style_mut().override_font_id = Some(eff_style.font.clone());
                let top = ui.max_rect().top();
                match self.kind {
                    PopoverKind::Calendar => self.calendar(ui, &eff_style),
                    PopoverKind::Volume => self.volume(ui, &eff_style, status, &mut out),
                    PopoverKind::Processes => self.processes(ui, &eff_style),
                    PopoverKind::Media => self.media(ui, &eff_style, status, &mut out),
                    PopoverKind::Network => self.network(ui, &eff_style, status, &mut out),
                    PopoverKind::Menu => self.menu(ui, &eff_style, &mut out),
                    PopoverKind::About => self.about(ui, &eff_style, &mut out),
                }
                // Content plus the frame's top and bottom padding.
                let h = (ui.min_rect().bottom() - top + 20.0).ceil();
                if self.content_h.is_none_or(|old| (old - h).abs() >= 1.0) {
                    self.content_h = Some(h);
                    pctx.request_repaint();
                }
            });
            let (focused, esc, close) = pctx.input(|i| {
                (i.viewport().focused, i.key_pressed(egui::Key::Escape), i.viewport().close_requested())
            });
            if focused == Some(true) {
                self.focused_once = true;
            }
            // Lost focus = clicked elsewhere. Ignore the first moments while focus settles.
            let lost = self.focused_once && focused == Some(false) && self.opened.elapsed() > Duration::from_millis(250);
            if esc || close || lost || clicked_outside || left_by_hover || out.close {
                out.close = false;
                request_dismiss = true;
            }
            if animating {
                pctx.request_repaint();
            }
        });

        if !self.shown {
            if crate::prepare_window_no_transitions(self.kind.title()) {
                self.shown = true;
            }
        }
        if request_dismiss {
            self.begin_close();
        }
        if close_t >= 1.0 {
            out.close = true;
        }
        // The clock / process list keep moving.
        if animating {
            ctx.request_repaint();
        } else {
            // Often enough to catch a click outside promptly.
            ctx.request_repaint_after(Duration::from_millis(60));
        }
        out
    }

    fn calendar(&mut self, ui: &mut egui::Ui, style: &PopoverStyle) {
        let now = local_now();
        let today = (i32::from(now.wYear), u32::from(now.wMonth), u32::from(now.wDay));
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::PageUp) {
                self.month = widgets::add_months(self.month.0, self.month.1, -1);
            }
            if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::PageDown) {
                self.month = widgets::add_months(self.month.0, self.month.1, 1);
            }
            if i.key_pressed(egui::Key::Home) || i.key_pressed(egui::Key::T) {
                self.month = (today.0, today.1);
            }
        });
        let (year, month) = self.month;
        ui.horizontal(|ui| {
            if ui.button("‹").clicked() {
                self.month = widgets::add_months(year, month, -1);
            }
            let label = format!("{} {}", widgets::MONTH_NAMES[(month - 1) as usize], year);
            let w = ui.available_width() - 28.0;
            ui.add_sized([w, 20.0], egui::Label::new(egui::RichText::new(label).color(style.fg)));
            if ui.button("›").clicked() {
                self.month = widgets::add_months(year, month, 1);
            }
        });
        ui.add_space(6.0);
        let cell = egui::vec2((ui.available_width() / 7.0).floor(), 30.0);
        let first = widgets::weekday_monday0(year, month, 1);
        let days = widgets::days_in_month(year, month);
        egui::Grid::new("cal").spacing([0.0, 0.0]).show(ui, |ui| {
            for d in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"] {
                ui.add_sized(cell, egui::Label::new(egui::RichText::new(d).color(style.subtext)));
            }
            ui.end_row();
            let mut col = 0;
            for _ in 0..first {
                ui.add_sized(cell, egui::Label::new(""));
                col += 1;
            }
            for day in 1..=days {
                let is_today = (year, month, day) == today;
                let (rect, _) = ui.allocate_exact_size(cell, egui::Sense::hover());
                if is_today {
                    ui.painter().rect_filled(rect.shrink(2.0), style.radius * 0.6, style.accent_bg);
                }
                let color = if is_today {
                    style.accent_fg
                } else if col >= 5 {
                    style.subtext
                } else {
                    style.fg
                };
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, day.to_string(), style.font.clone(), color);
                col += 1;
                if col == 7 {
                    col = 0;
                    ui.end_row();
                }
            }
        });
        if (year, month) != (today.0, today.1) {
            ui.add_space(4.0);
            if ui.small_button("Today").clicked() {
                self.month = (today.0, today.1);
            }
        }
    }

    fn processes(&mut self, ui: &mut egui::Ui, style: &PopoverStyle) {
        let rows = self.procs.as_ref().map(|f| f.rows.lock().clone()).unwrap_or_default();
        if rows.is_empty() {
            ui.label(egui::RichText::new("Measuring…").color(style.subtext));
            return;
        }
        let table = |ui: &mut egui::Ui, heading: &str, items: Vec<ProcRow>, cpu: bool| {
            ui.label(egui::RichText::new(heading).color(style.subtext));
            ui.add_space(2.0);
            egui::Grid::new(heading).num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                for r in items {
                    let name = if r.count > 1 { format!("{} ({})", r.name, r.count) } else { r.name.clone() };
                    let name: String = name.chars().take(28).collect();
                    ui.allocate_ui_with_layout(
                        egui::vec2(250.0, 18.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.set_width(250.0);
                            ui.add(egui::Label::new(name).truncate());
                        },
                    );
                    let value = if cpu {
                        format!("{:>5.1}%", r.cpu)
                    } else if r.mem_mb >= 1024.0 {
                        format!("{:>5.1} GB", r.mem_mb / 1024.0)
                    } else {
                        format!("{:>5.0} MB", r.mem_mb)
                    };
                    ui.label(value);
                    ui.end_row();
                }
            });
        };
        table(ui, "CPU", procs::top(&rows, 5, |r| r.cpu), true);
        ui.add_space(10.0);
        table(ui, "MEMORY", procs::top(&rows, 5, |r| r.mem_mb), false);
    }

    fn volume(&mut self, ui: &mut egui::Ui, style: &PopoverStyle, status: &Status, out: &mut Outcome) {
        let Some(vol) = status.volume else {
            ui.label(egui::RichText::new("No output device").color(style.subtext));
            return;
        };
        let n_out = status.outputs.len();
        if n_out > 0 {
            self.selected_idx = self.selected_idx.min(n_out - 1);
        }
        ui.input(|i| {
            if i.key_pressed(egui::Key::M) || i.key_pressed(egui::Key::Space) {
                out.cmds.push(Cmd::ToggleMute);
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                out.cmds.push(Cmd::Adjust(-0.05));
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                out.cmds.push(Cmd::Adjust(0.05));
            }
            if n_out > 0 {
                if i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::Tab) {
                    self.selected_idx = (self.selected_idx + 1) % n_out;
                }
                if i.key_pressed(egui::Key::ArrowUp) {
                    self.selected_idx = (self.selected_idx + n_out - 1) % n_out;
                }
                if i.key_pressed(egui::Key::Enter) {
                    if let Some(o) = status.outputs.get(self.selected_idx) {
                        out.cmds.push(Cmd::SetOutput(o.id.clone()));
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            let mute_label = if vol.muted { "Unmute" } else { "Mute" };
            if ui.button(mute_label).clicked() {
                out.cmds.push(Cmd::ToggleMute);
            }
            let mut pct = (vol.level * 100.0).round();
            ui.spacing_mut().slider_width = ui.available_width() - 56.0;
            let resp = ui.add(egui::Slider::new(&mut pct, 0.0..=100.0).show_value(false));
            if resp.changed() {
                out.cmds.push(Cmd::SetVolume(pct / 100.0));
            }
            ui.label(format!("{pct:>3.0}%"));
        });
        ui.add_space(10.0);
        ui.label(egui::RichText::new("OUTPUT").color(style.subtext));
        ui.add_space(2.0);
        for (idx, o) in status.outputs.iter().enumerate() {
            let mark = match (style.nerd, o.default) {
                (true, true) => "\u{f043e}",
                (true, false) => "\u{f043d}",
                (false, true) => "(*)",
                (false, false) => "( )",
            };
            let name: String = o.name.chars().take(34).collect();
            let text = egui::RichText::new(format!("{mark} {name}"));
            let is_sel = o.default || idx == self.selected_idx;
            let resp = ui
                .add_sized(
                    [ui.available_width(), 24.0],
                    egui::Button::new(text).selected(is_sel).frame(true).truncate(),
                )
                .hover_card(o.name.clone());
            if resp.clicked() && !o.default {
                self.selected_idx = idx;
                out.cmds.push(Cmd::SetOutput(o.id.clone()));
            }
        }
        ui.add_space(6.0);
        if ui.small_button("Sound settings…").clicked() {
            out.open_uri = Some("ms-settings:sound");
            out.close = true;
        }
    }

    fn media(&mut self, ui: &mut egui::Ui, style: &PopoverStyle, status: &Status, out: &mut Outcome) {
        let Some(m) = &status.media else {
            ui.label(egui::RichText::new("Nothing playing").color(style.subtext));
            return;
        };

        // Keyboard shortcuts in media popover
        ui.input(|i| {
            if i.key_pressed(egui::Key::Space) || i.key_pressed(egui::Key::K) {
                out.cmds.push(Cmd::PlayPause);
            }
            if i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::J) {
                out.cmds.push(Cmd::Previous);
            }
            if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::L) {
                out.cmds.push(Cmd::Next);
            }
            if i.key_pressed(egui::Key::Tab) && status.media_players.len() > 1 {
                let cur_idx = status
                    .media_players
                    .iter()
                    .position(|p| p.app == m.app)
                    .unwrap_or(0);
                let next_idx = (cur_idx + 1) % status.media_players.len();
                out.cmds
                    .push(Cmd::SelectPlayer(status.media_players[next_idx].app.clone()));
            }
        });

        // Per-player selector pills when multiple sessions exist (or show active player pill)
        if status.media_players.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                for p in &status.media_players {
                    let short = short_app_name(&p.app);
                    let active = p.app == m.app;
                    let btn = egui::Button::new(
                        egui::RichText::new(if active { format!("● {short}") } else { short.to_owned() }).color(
                            if active { style.accent_fg } else { style.subtext },
                        ),
                    )
                    .fill(if active {
                        style.accent_bg
                    } else {
                        style.accent_bg.gamma_multiply(0.16)
                    })
                    .corner_radius(style.radius * 0.6);
                    if ui.add(btn).clicked() && !active {
                        out.cmds.push(Cmd::SelectPlayer(p.app.clone()));
                    }
                }
            });
            ui.add_space(6.0);
        }

        // Album art card with theme accent tint + track metadata
        ui.horizontal(|ui| {
            let (art_rect, _) = ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
            let art_bg = blend_color(style.bg, style.accent_bg, 0.65);
            ui.painter()
                .rect_filled(art_rect, (style.radius * 0.7).max(6.0), art_bg);
            ui.painter().rect_stroke(
                art_rect,
                (style.radius * 0.7).max(6.0),
                egui::Stroke::new(1.0_f32, style.accent_bg),
                egui::StrokeKind::Inside,
            );
            let emblem = if !m.album.is_empty() {
                m.album.chars().next().unwrap_or('♪').to_uppercase().to_string()
            } else if style.nerd {
                "\u{f075a}".to_owned()
            } else {
                "♪".to_owned()
            };
            ui.painter().text(
                art_rect.center(),
                egui::Align2::CENTER_CENTER,
                emblem,
                style.font.clone(),
                style.accent_fg,
            );

            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.add(egui::Label::new(egui::RichText::new(&m.title).color(style.fg)).truncate());
                let app = short_app_name(&m.app);
                let sub = match (m.artist.is_empty(), app.is_empty()) {
                    (false, false) => format!("{} · {app}", m.artist),
                    (false, true) => m.artist.clone(),
                    (true, _) => app.to_owned(),
                };
                ui.add(egui::Label::new(egui::RichText::new(sub).color(style.subtext)).truncate());
            });
        });

        ui.add_space(8.0);
        // Seek bar
        let dur = m.duration_secs.max(0.0);
        let mut pos = m.position_secs.clamp(0.0, dur.max(1.0));
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(fmt_mm_ss(pos)).color(style.subtext));
            let slider_w = (ui.available_width() - 48.0).max(60.0);
            ui.spacing_mut().slider_width = slider_w;
            let resp = ui.add_enabled(
                dur > 0.5,
                egui::Slider::new(&mut pos, 0.0..=dur.max(1.0)).show_value(false),
            );
            if resp.changed() && dur > 0.5 {
                out.cmds.push(Cmd::SeekMedia(pos));
            }
            ui.label(egui::RichText::new(fmt_mm_ss(dur)).color(style.subtext));
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 16.0) / 3.0;
            let draw_btn = |ui: &mut egui::Ui, painter_fn: fn(&egui::Painter, egui::Rect, egui::Color32)| -> egui::Response {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 28.0), egui::Sense::click());
                let bg = if resp.is_pointer_button_down_on() {
                    style.accent_bg.gamma_multiply(0.4)
                } else if resp.hovered() {
                    style.accent_bg.gamma_multiply(0.2)
                } else {
                    style.bg
                };
                ui.painter().rect_filled(rect, 4.0, bg);
                ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(1.0_f32, style.border), egui::StrokeKind::Inside);
                painter_fn(ui.painter(), rect, style.fg);
                resp
            };

            if draw_btn(ui, chickenwm_core::ui_icons::paint_media_prev).clicked() {
                out.cmds.push(Cmd::Previous);
            }
            let play_fn = if m.playing {
                chickenwm_core::ui_icons::paint_media_pause
            } else {
                chickenwm_core::ui_icons::paint_media_play
            };
            if draw_btn(ui, play_fn).clicked() {
                out.cmds.push(Cmd::PlayPause);
            }
            if draw_btn(ui, chickenwm_core::ui_icons::paint_media_next).clicked() {
                out.cmds.push(Cmd::Next);
            }
        });
    }

    fn menu(&mut self, ui: &mut egui::Ui, style: &PopoverStyle, out: &mut Outcome) {
        let n = MenuAction::ALL.len();
        self.selected_idx = self.selected_idx.min(n - 1);
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::Tab) {
                self.selected_idx = (self.selected_idx + 1) % n;
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                self.selected_idx = (self.selected_idx + n - 1) % n;
            }
            if i.key_pressed(egui::Key::Enter) {
                out.menu = Some(MenuAction::ALL[self.selected_idx]);
                out.close = true;
            }
        });
        // Rows sit flush; a faint line separates each from the next.
        ui.spacing_mut().item_spacing.y = 0.0;
        let separator = egui::Stroke::new(1.0_f32, style.border.gamma_multiply(0.35));
        for (idx, action) in MenuAction::ALL.into_iter().enumerate() {
            if idx > 0 {
                let (line, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 5.0), egui::Sense::hover());
                let y = line.center().y.round() + 0.5;
                ui.painter().hline(line.x_range().shrink(4.0), y, separator);
            }
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::hover());
            let resp = ui.interact(rect, ui.id().with(("menu-row", idx)), egui::Sense::click());
            if idx == self.selected_idx {
                ui.painter().rect_filled(rect, (style.radius - 2.0).max(2.0), style.accent_bg.gamma_multiply(0.35));
            }
            ui.painter().text(
                rect.left_center() + egui::vec2(10.0, 0.0),
                egui::Align2::LEFT_CENTER,
                action.label(),
                style.font.clone(),
                style.fg,
            );
            if resp.hovered() {
                self.selected_idx = idx;
            }
            if resp.clicked() {
                out.menu = Some(action);
                out.close = true;
            }
        }
    }

    fn about(&mut self, ui: &mut egui::Ui, style: &PopoverStyle, out: &mut Outcome) {
        ui.label(egui::RichText::new("ChickenWM").color(style.fg).size(style.font.size * 1.5));
        ui.label(egui::RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(style.subtext));
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("A Hyprland-style tiling window manager and desktop shell for Windows.")
                .color(style.fg),
        );
        ui.add_space(8.0);
        let link = ui
            .add(egui::Label::new(egui::RichText::new("github.com/despale-code/chickenwm").underline()).sense(egui::Sense::click()))
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if link.clicked() {
            out.open_uri = Some("https://github.com/despale-code/chickenwm");
            out.close = true;
        }
    }

    fn network(&mut self, ui: &mut egui::Ui, style: &PopoverStyle, status: &Status, out: &mut Outcome) {
        let count = status.wifi_networks.len();
        if count > 0 {
            self.selected_idx = self.selected_idx.min(count - 1);
        }
        ui.input(|i| {
            if i.key_pressed(egui::Key::R) {
                out.cmds.push(Cmd::RefreshWifi);
            }
            if count > 0 {
                if i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::Tab) {
                    self.selected_idx = (self.selected_idx + 1) % count;
                }
                if i.key_pressed(egui::Key::ArrowUp) {
                    self.selected_idx = (self.selected_idx + count - 1) % count;
                }
                if i.key_pressed(egui::Key::Enter) {
                    if let Some(net) = status.wifi_networks.get(self.selected_idx) {
                        let target = if net.profile.is_empty() { &net.ssid } else { &net.profile };
                        out.cmds.push(Cmd::ConnectWifi(target.clone()));
                    }
                }
            }
        });

        match &status.network {
            crate::status::Network::Ethernet {
                adapter,
                speed_mbps,
                ip,
            } => {
                ui.label(egui::RichText::new(format!("Ethernet · {adapter}")).color(style.fg));
                ui.label(
                    egui::RichText::new(format!(
                        "{} Mbps • IP {}",
                        speed_mbps,
                        if ip.is_empty() { "—" } else { ip }
                    ))
                    .color(style.subtext),
                );
            }
            crate::status::Network::Wifi {
                ssid,
                signal,
                speed_mbps,
                ip,
            } => {
                let name = if ssid.is_empty() { "Connected" } else { ssid.as_str() };
                ui.label(egui::RichText::new(format!("Wi-Fi · {name} ({signal}%)")).color(style.fg));
                ui.label(
                    egui::RichText::new(format!(
                        "{} Mbps • IP {}",
                        speed_mbps,
                        if ip.is_empty() { "—" } else { ip }
                    ))
                    .color(style.subtext),
                );
            }
            crate::status::Network::Disconnected => {
                ui.label(egui::RichText::new("Not connected").color(style.subtext));
            }
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("WI-FI NETWORKS").color(style.subtext));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Refresh").clicked() {
                    out.cmds.push(Cmd::RefreshWifi);
                }
            });
        });
        ui.add_space(2.0);

        if status.wifi_networks.is_empty() {
            ui.label(egui::RichText::new("No Wi-Fi networks found").color(style.subtext));
        } else {
            egui::ScrollArea::vertical().max_height(190.0).show(ui, |ui| {
                for (idx, net) in status.wifi_networks.iter().enumerate() {
                    let bars = if style.nerd {
                        match net.signal {
                            0..=24 => "\u{f091f}", // 1 bar
                            25..=49 => "\u{f0922}", // 2 bars
                            50..=74 => "\u{f0925}", // 3 bars
                            _ => "\u{f0928}", // 4 bars
                        }
                    } else {
                        match net.signal {
                            0..=24 => "[•   ]",
                            25..=49 => "[••  ]",
                            50..=74 => "[••• ]",
                            _ => "[••••]",
                        }
                    };
                    let lock = if net.secured { if style.nerd { "\u{f023}" } else { "🔒" } } else { "  " };
                    let state = if net.connected { " (connected)" } else { "" };
                    let label = format!("{bars} {lock} {}{state}  {}%", net.ssid, net.signal);
                    let is_sel = net.connected || idx == self.selected_idx;
                    let resp = ui.add_sized(
                        [ui.available_width(), 24.0],
                        egui::Button::new(egui::RichText::new(label))
                            .selected(is_sel)
                            .frame(true)
                            .truncate(),
                    );
                    if resp.clicked() {
                        self.selected_idx = idx;
                        if !net.connected {
                            let target = if net.profile.is_empty() { &net.ssid } else { &net.profile };
                            out.cmds.push(Cmd::ConnectWifi(target.clone()));
                        }
                    }
                }
            });
        }

        ui.add_space(6.0);
        if ui.small_button("Network settings…").clicked() {
            out.open_uri = Some("ms-settings:network-wifi");
            out.close = true;
        }
    }
}

fn short_app_name(app: &str) -> &str {
    let base = app
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(app)
        .trim_end_matches(".exe");
    base.split('!').next().unwrap_or(base)
}

fn fmt_mm_ss(secs: f32) -> String {
    let total = secs.max(0.0).round() as u32;
    format!("{}:{:02}", total / 60, total % 60)
}

fn blend_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    egui::Color32::from_rgba_unmultiplied(
        (a.r() as f32 * inv + b.r() as f32 * t).round() as u8,
        (a.g() as f32 * inv + b.g() as f32 * t).round() as u8,
        (a.b() as f32 * inv + b.b() as f32 * t).round() as u8,
        a.a(),
    )
}

fn local_now() -> windows::Win32::Foundation::SYSTEMTIME {
    unsafe { windows::Win32::System::SystemInformation::GetLocalTime() }
}
