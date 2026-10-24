//! `roobar` — Waybar-Style Status Bar for ChickenWM
//! - Directly synchronized with `chickenwm-wm` via `\\.\pipe\chickenwm-bar-ipc` (`NotifyBarState` push)
//!   and `\\.\pipe\chickenwm-wm-ipc` (`QueryBarState` / `Dispatch(Action::Workspace)`).
//! - Styled via hot-reloading `~/.config/chickenwm/roobar.css` (`<5ms` `notify` reload).
//! - Modules:
//!   * Left: `#custom-launcher`, `#workspaces` (`1..10` with `.active`, `.occupied`, `.empty` states,
//!     animated pill expansion, click-to-switch, scroll-wheel workspace cycling), `#layout`
//!   * Center: `#window` (active window process badge & title on current workspace)
//!   * Right: a configurable list from the widget library (`widgets.rs`, `--modules-right` in
//!     roobar.css): stats, cpu, memory, refresh-rate, network, volume, bluetooth, files, settings,
//!     clock, power

// Release builds are windowed apps: no console window when launched from the shell, a
// hotkey or the launcher. Debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use eframe::egui;
use chickenwm_core::bootstrap_config_and_themes;
use chickenwm_core::config::RgbaColor;
use chickenwm_core::css::{load_css_sheet, spawn_config_watcher, ConfigWatchEvent, CssStyleSheet};
use chickenwm_core::failsafe::spawn_emergency_failsafe_thread;
use chickenwm_core::init_tracing;
use chickenwm_core::ipc::{
    send_ipc_command_blocking, serve_ipc, Action, BarStateSnapshot, IpcCommand, IpcResponse,
    PIPE_NAME_BAR, PIPE_NAME_DAEMON, PIPE_NAME_WM,
};
use chickenwm_core::monitor::enumerate_monitors;
use parking_lot::Mutex;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

mod hovercard;
mod popover;
mod previews;
mod procs;
mod tray;
mod status;
mod widgets;

use hovercard::HoverCardExt;
use popover::{Popover, PopoverKind};
use status::{Cmd, StatusMonitor};
use widgets::Widget;
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::SystemInformation::{
    GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::GetSystemTimes;

fn to_color32(c: RgbaColor) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

#[derive(Default)]
struct CpuTracker {
    prev_idle: u64,
    prev_kernel: u64,
    prev_user: u64,
    usage_pct: f32,
}

impl CpuTracker {
    fn sample(&mut self) -> f32 {
        unsafe {
            let mut idle = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
                let i = ((idle.dwHighDateTime as u64) << 32) | (idle.dwLowDateTime as u64);
                let k = ((kernel.dwHighDateTime as u64) << 32) | (kernel.dwLowDateTime as u64);
                let u = ((user.dwHighDateTime as u64) << 32) | (user.dwLowDateTime as u64);

                let di = i.saturating_sub(self.prev_idle);
                let dk = k.saturating_sub(self.prev_kernel);
                let du = u.saturating_sub(self.prev_user);
                self.prev_idle = i;
                self.prev_kernel = k;
                self.prev_user = u;

                let total = dk + du;
                if total > 0 {
                    let busy = total.saturating_sub(di);
                    self.usage_pct = ((busy as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32;
                }
            }
        }
        self.usage_pct
    }
}

fn query_memory_stats() -> (f32, f32, u32) {
    unsafe {
        let mut mem = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if GlobalMemoryStatusEx(&mut mem).is_ok() {
            let total_gb = mem.ullTotalPhys as f32 / (1024.0 * 1024.0 * 1024.0);
            let avail_gb = mem.ullAvailPhys as f32 / (1024.0 * 1024.0 * 1024.0);
            let used_gb = (total_gb - avail_gb).max(0.0);
            return (used_gb, total_gb, mem.dwMemoryLoad);
        }
    }
    (0.0, 0.0, 0)
}

fn format_local_clock(fmt: &str) -> String {
    let st: SYSTEMTIME = unsafe { GetLocalTime() };
    widgets::format_clock(fmt, &st)
}

/// Rounds `size` to the nearest whole multiple of the font's pixel grid in physical pixels.
/// Pixel fonts (DepartureMono: 11px) only render crisply at exact multiples; anything between
/// gets anti-aliased into blur. `grid <= 0` disables snapping.
fn snap_font_size(size: f32, grid: f32, pixels_per_point: f32) -> f32 {
    if grid <= 0.0 {
        return size;
    }
    let multiple = (size * pixels_per_point / grid).round().max(1.0);
    multiple * grid / pixels_per_point
}

/// Colours shared by every island in the bar.
struct IslandStyle {
    bg: egui::Color32,
    hover_bg: egui::Color32,
    border: egui::Color32,
    radius: f32,
    /// Faux-bold text (`--font-weight: bold`): pixel fonts like DepartureMono ship no bold face.
    bold: bool,
    /// `--islands: false`: the bar has one background and border; islands keep their layout
    /// but draw only a hover highlight.
    flat: bool,
}

/// Draws `galley` at `pos`; bold double-strikes it 1px to the right, which reads as a crisp
/// bold weight on pixel fonts.
fn draw_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    galley: Arc<egui::Galley>,
    color: egui::Color32,
    bold: bool,
) {
    if bold {
        painter.galley(pos + egui::vec2(1.0, 0.0), Arc::clone(&galley), color);
    }
    painter.galley(pos, galley, color);
}

/// Paints one floating island, flat: a solid fill (blended toward `hover_bg` by `hover_t`) and
/// a hairline border. No shadows or gradients.
fn paint_island(painter: &egui::Painter, rect: egui::Rect, style: &IslandStyle, hover_t: f32) {
    if style.flat {
        paint_hover_highlight(painter, rect, style, hover_t);
        return;
    }
    let fill = style.bg.lerp_to_gamma(style.hover_bg, hover_t);
    painter.rect(rect, style.radius, fill, egui::Stroke::new(1.0_f32, style.border), egui::StrokeKind::Inside);
}
/// The hover fill of a segment that has no island of its own (in a shared pill, or `flat`).
fn paint_hover_highlight(painter: &egui::Painter, rect: egui::Rect, style: &IslandStyle, hover_t: f32) {
    if hover_t > 0.0 {
        let fill = style.hover_bg.gamma_multiply(hover_t);
        painter.rect_filled(rect.shrink(3.0), (style.radius - 3.0).max(0.0), fill);
    }
}

/// An island holding a single line of text. Clickable islands fade toward the hover colour.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
fn island_label(
    ui: &mut egui::Ui,
    id: egui::Id,
    style: &IslandStyle,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    height: f32,
    clickable: bool,
) -> egui::Response {
    pill_label(ui, id, style, text, font, color, height, clickable, false)
}

/// An island displaying an optional vector icon and optional text label.
#[allow(clippy::too_many_arguments)]
fn island_vector<F>(
    ui: &mut egui::Ui,
    id: egui::Id,
    style: &IslandStyle,
    text: Option<&str>,
    font: egui::FontId,
    color: egui::Color32,
    height: f32,
    clickable: bool,
    paint_icon: Option<F>,
) -> egui::Response
where
    F: FnOnce(&egui::Painter, egui::Rect, egui::Color32),
{
    pill_vector(ui, id, style, text, font, color, height, clickable, false, paint_icon)
}

/// `island_label`, or with `in_group` one segment of a shared pill: no island of its own, just a
/// hover highlight, and tighter padding.
#[allow(clippy::too_many_arguments)]
fn pill_label(
    ui: &mut egui::Ui,
    id: egui::Id,
    style: &IslandStyle,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
    height: f32,
    clickable: bool,
    in_group: bool,
) -> egui::Response {
    pill_vector::<fn(&egui::Painter, egui::Rect, egui::Color32)>(
        ui, id, style, Some(text), font, color, height, clickable, in_group, None,
    )
}

/// An island or group segment displaying an optional vector icon and optional text label.
#[allow(clippy::too_many_arguments)]
fn pill_vector<F>(
    ui: &mut egui::Ui,
    id: egui::Id,
    style: &IslandStyle,
    text: Option<&str>,
    font: egui::FontId,
    color: egui::Color32,
    height: f32,
    clickable: bool,
    in_group: bool,
    paint_icon: Option<F>,
) -> egui::Response
where
    F: FnOnce(&egui::Painter, egui::Rect, egui::Color32),
{
    let bold_extra = if style.bold { 1.0 } else { 0.0 };
    let icon_w = if paint_icon.is_some() { 15.0_f32 } else { 0.0_f32 };
    let has_text = text.map_or(false, |t| !t.is_empty());
    let gap = if paint_icon.is_some() && has_text { 6.0_f32 } else { 0.0_f32 };

    let (galley, text_w) = if let Some(t) = text.filter(|t| !t.is_empty()) {
        let g = ui.painter().layout_no_wrap(t.to_owned(), font, color);
        let w = g.size().x;
        (Some(g), w)
    } else {
        (None, 0.0_f32)
    };

    let content_w = icon_w + gap + text_w + bold_extra;
    let padding = if in_group {
        if galley.is_none() { 12.0 } else { 16.0 }
    } else if galley.is_none() {
        height * 0.45
    } else {
        22.0
    };
    let width = (content_w + padding).max(if in_group { 18.0 } else { height + 4.0 });
    let size = egui::vec2(width, height);
    let sense = if clickable {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let hover_t = ui
        .ctx()
        .animate_bool_with_time(id, clickable && resp.hovered(), 0.15);
    if in_group {
        paint_hover_highlight(ui.painter(), rect, style, hover_t);
    } else {
        paint_island(ui.painter(), rect, style, hover_t);
    }

    let start_x = rect.center().x - content_w / 2.0;
    if let Some(painter_fn) = paint_icon {
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(start_x + icon_w / 2.0, rect.center().y),
            egui::vec2(icon_w, icon_w),
        );
        painter_fn(ui.painter(), icon_rect, color);
    }
    if let Some(g) = galley {
        let text_pos = egui::pos2(
            start_x + icon_w + gap,
            rect.center().y - g.size().y / 2.0,
        );
        draw_text(ui.painter(), text_pos, g, color, style.bold);
    }
    resp
}

/// Launches another ChickenWM executable, preferring the copy next to this one.
fn spawn_sibling(exe_name: &str) {
    if let Ok(mut exe) = std::env::current_exe() {
        exe.set_file_name(exe_name);
        if Command::new(&exe).spawn().is_ok() {
            return;
        }
    }
    let _ = Command::new(exe_name).spawn();
}

#[derive(Clone, Copy, PartialEq)]
enum PowerAction {
    Shutdown,
    Restart,
    LogOut,
}

impl PowerAction {
    const ALL: [PowerAction; 3] = [PowerAction::Shutdown, PowerAction::Restart, PowerAction::LogOut];

    fn label(self) -> &'static str {
        match self {
            PowerAction::Shutdown => "Shut down",
            PowerAction::Restart => "Restart",
            PowerAction::LogOut => "Log out",
        }
    }
}

/// Which built-in gesture handlers a widget should still run this frame: a gesture that has an
/// override in roobar.css (`--<widget>-on-<gesture>`) runs that instead and reads as false here.
struct Gestures {
    click: bool,
    middle: bool,
    right: bool,
    /// Scroll delta for the built-in handler (0 when there is none or it was overridden).
    scroll: f32,
}

fn gestures(ui: &egui::Ui, css: &CssStyleSheet, widget: Widget, resp: &egui::Response) -> Gestures {
    use widgets::Gesture;
    let take = |happened: bool, g: Gesture| -> bool {
        if !happened {
            return false;
        }
        match widgets::gesture_override(css, widget, g) {
            Some(action) => {
                run_gesture_action(action);
                false
            }
            None => true,
        }
    };
    let click = take(resp.clicked(), Gesture::Click);
    let middle = take(resp.middle_clicked(), Gesture::Middle);
    let right = take(resp.secondary_clicked(), Gesture::Right);
    let raw = if resp.hovered() { ui.input(|i| i.raw_scroll_delta.y) } else { 0.0 };
    let scroll = if raw.abs() <= 0.5 {
        0.0
    } else if take(true, if raw > 0.0 { Gesture::ScrollUp } else { Gesture::ScrollDown }) {
        raw
    } else {
        0.0
    };
    Gestures { click, middle, right, scroll }
}

fn run_gesture_action(action: widgets::GestureAction) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    match action {
        widgets::GestureAction::None => {}
        widgets::GestureAction::Open(target) => status::open_uri(&target),
        widgets::GestureAction::Exec(cmdline) => {
            // Through cmd so PATH lookup, quoting and arguments behave as typed.
            if let Err(e) = Command::new("cmd.exe").raw_arg(format!("/C start \"\" {cmdline}")).creation_flags(CREATE_NO_WINDOW).spawn() {
                tracing::warn!("bar gesture `{cmdline}` failed: {e}");
            }
        }
        widgets::GestureAction::Dispatch(dispatcher, arg) => match Action::parse_dispatcher(&dispatcher, &arg) {
            Some(action) => {
                std::thread::spawn(move || {
                    if send_ipc_command_blocking(PIPE_NAME_WM, &IpcCommand::Dispatch(action.clone())).is_err() {
                        let _ = send_ipc_command_blocking(PIPE_NAME_DAEMON, &IpcCommand::Dispatch(action));
                    }
                });
            }
            None => tracing::warn!("bar gesture: unknown dispatcher `{dispatcher} {arg}`"),
        },
    }
}

fn run_power_action(action: PowerAction) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // `/l` rejects a `/t` timeout.
    let args: &[&str] = match action {
        PowerAction::Shutdown => &["/s", "/t", "0"],
        PowerAction::Restart => &["/r", "/t", "0"],
        PowerAction::LogOut => &["/l"],
    };
    if let Err(e) = Command::new("shutdown.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        tracing::warn!("shutdown.exe {args:?} failed: {e}");
    }
}

/// Three stacked horizontal bars (☰), drawn as shapes like the power glyph.
fn paint_menu_glyph(painter: &egui::Painter, center: egui::Pos2, half_w: f32, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.6_f32, color);
    let gap = (half_w * 0.62).round();
    for dy in [-gap, 0.0, gap] {
        let y = (center.y + dy).round() + 0.5;
        painter.line_segment([egui::pos2(center.x - half_w, y), egui::pos2(center.x + half_w, y)], stroke);
    }
}

/// The IEC power symbol (⏻): a ring open at the top with a bar through the gap. Drawn as
/// shapes so it doesn't depend on the bar font having the glyph.
fn paint_power_glyph(painter: &egui::Painter, center: egui::Pos2, r: f32, color: egui::Color32) {
    let stroke = egui::Stroke::new((r * 0.22).max(1.5), color);
    let gap = 0.75_f32; // radians either side of 12 o'clock
    let start = -std::f32::consts::FRAC_PI_2 + gap;
    let sweep = std::f32::consts::TAU - 2.0 * gap;
    let steps = 32;
    let ring = (0..=steps)
        .map(|i| {
            let a = start + sweep * i as f32 / steps as f32;
            center + egui::vec2(a.cos(), a.sin()) * r
        })
        .collect();
    painter.add(egui::Shape::line(ring, stroke));
    painter.line_segment(
        [center - egui::vec2(0.0, r * 1.15), center - egui::vec2(0.0, r * 0.15)],
        stroke,
    );
}

fn glyph_stroke(r: f32, color: egui::Color32) -> egui::Stroke {
    egui::Stroke::new((r * 0.22).max(1.5), color)
}

/// Circular arrow open at the top right, arrowhead at 12 o'clock pointing clockwise.
fn paint_restart_glyph(painter: &egui::Painter, center: egui::Pos2, r: f32, color: egui::Color32) {
    let start = -std::f32::consts::FRAC_PI_2 + 1.0;
    let end = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU;
    let steps = 32;
    let arc = (0..=steps)
        .map(|i| {
            let a = start + (end - start) * i as f32 / steps as f32;
            center + egui::vec2(a.cos(), a.sin()) * r
        })
        .collect();
    painter.add(egui::Shape::line(arc, glyph_stroke(r, color)));
    let tip = center + egui::vec2(0.0, -r);
    painter.add(egui::Shape::convex_polygon(
        vec![
            tip + egui::vec2(-r * 0.1, -r * 0.42),
            tip + egui::vec2(r * 0.48, 0.0),
            tip + egui::vec2(-r * 0.1, r * 0.42),
        ],
        color,
        egui::Stroke::NONE,
    ));
}

/// Open door bracket with an arrow leaving through it.
fn paint_logout_glyph(painter: &egui::Painter, center: egui::Pos2, r: f32, color: egui::Color32) {
    let stroke = glyph_stroke(r, color);
    let p = |x: f32, y: f32| center + egui::vec2(x * r, y * r);
    painter.add(egui::Shape::line(
        vec![p(0.05, -1.0), p(-0.9, -1.0), p(-0.9, 1.0), p(0.05, 1.0)],
        stroke,
    ));
    painter.line_segment([p(-0.35, 0.0), p(1.0, 0.0)], stroke);
    painter.add(egui::Shape::line(vec![p(0.55, -0.45), p(1.0, 0.0), p(0.55, 0.45)], stroke));
}

/// Full-screen power overlay: dims the live desktop and offers the `PowerAction`s as cards.
///
/// It's a real transparent window (no screenshot). Two details keep it seamless:
/// - it's 1 px shorter than the monitor, because Windows treats a window covering the exact
///   monitor as fullscreen and stops compositing its transparent pixels (they turn black);
/// - it paints nothing until its DWM open/close transition is disabled, so Windows' own
///   "new window" zoom/fade has nothing to show; the only motion is ours. (It is created
///   visible: a transparent window that starts hidden can come up opaque black.)
struct PowerOverlay {
    opened: Instant,
    /// Set once dismissed or an option is picked; the overlay fades out, then runs it.
    closing: Option<(Instant, Option<PowerAction>)>,
    /// False until the window exists and has its DWM transitions disabled.
    shown: bool,
    /// The monitor as it looked when the overlay opened (sharp + blurred). The overlay paints
    /// this itself instead of relying on window transparency, which a near-fullscreen GL
    /// window doesn't reliably keep (it came out opaque black).
    backdrop: Option<Backdrop>,
}

pub(crate) struct Backdrop {
    pub sharp: egui::TextureHandle,
    pub blurred: egui::TextureHandle,
}

/// Copies the screen area `[x, y, w, h]` (physical pixels) into an image.
fn capture_screen(x: i32, y: i32, w: i32, h: i32) -> Option<egui::ColorImage> {
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        SRCCOPY,
    };
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        let copied = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY).is_ok();
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(mem, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if !copied || lines == 0 {
            return None;
        }
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
            px[3] = 255;
        }
        Some(egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &buf))
    }
}

/// A small, heavily blurred copy of `img`: averaged down by `factor`, then box-blurred. Drawn
/// stretched with linear filtering it becomes a smooth frosted backdrop.
fn blurred_thumbnail(img: &egui::ColorImage, factor: usize) -> egui::ColorImage {
    let [w, h] = img.size;
    let (sw, sh) = ((w / factor).max(1), (h / factor).max(1));
    let mut acc = vec![[0f32; 3]; sw * sh];
    for sy in 0..sh {
        for sx in 0..sw {
            let mut sum = [0u32; 3];
            let mut n = 0u32;
            // Sparse sampling inside each block is plenty before the blur passes.
            for y in (sy * factor..((sy + 1) * factor).min(h)).step_by(2) {
                for x in (sx * factor..((sx + 1) * factor).min(w)).step_by(2) {
                    let p = img.pixels[y * w + x];
                    sum[0] += p.r() as u32;
                    sum[1] += p.g() as u32;
                    sum[2] += p.b() as u32;
                    n += 1;
                }
            }
            let n = n.max(1) as f32;
            acc[sy * sw + sx] = [sum[0] as f32 / n, sum[1] as f32 / n, sum[2] as f32 / n];
        }
    }
    // Three separable box passes ≈ a gaussian.
    const R: isize = 2;
    let mut tmp = acc.clone();
    for _ in 0..3 {
        for y in 0..sh {
            for x in 0..sw {
                let mut s = [0f32; 3];
                let mut n = 0f32;
                for d in -R..=R {
                    let xx = x as isize + d;
                    if xx >= 0 && (xx as usize) < sw {
                        let p = acc[y * sw + xx as usize];
                        s = [s[0] + p[0], s[1] + p[1], s[2] + p[2]];
                        n += 1.0;
                    }
                }
                tmp[y * sw + x] = [s[0] / n, s[1] / n, s[2] / n];
            }
        }
        for y in 0..sh {
            for x in 0..sw {
                let mut s = [0f32; 3];
                let mut n = 0f32;
                for d in -R..=R {
                    let yy = y as isize + d;
                    if yy >= 0 && (yy as usize) < sh {
                        let p = tmp[yy as usize * sw + x];
                        s = [s[0] + p[0], s[1] + p[1], s[2] + p[2]];
                        n += 1.0;
                    }
                }
                acc[y * sw + x] = [s[0] / n, s[1] / n, s[2] / n];
            }
        }
    }
    let pixels = acc
        .iter()
        .map(|p| egui::Color32::from_rgb(p[0] as u8, p[1] as u8, p[2] as u8))
        .collect();
    egui::ColorImage { size: [sw, sh], pixels }
}

impl Backdrop {
    /// Captures the monitor under `rect` (egui points) before the overlay covers it.
    pub(crate) fn capture(ctx: &egui::Context, rect: egui::Rect) -> Option<Self> {
        let ppp = ctx.pixels_per_point();
        let img = capture_screen(
            (rect.min.x * ppp).round() as i32,
            (rect.min.y * ppp).round() as i32,
            (rect.width() * ppp).round() as i32,
            (rect.height() * ppp).round() as i32,
        )?;
        let blurred = blurred_thumbnail(&img, 14);
        Some(Self {
            sharp: ctx.load_texture("power-backdrop-sharp", img, egui::TextureOptions::LINEAR),
            blurred: ctx.load_texture("power-backdrop-blur", blurred, egui::TextureOptions::LINEAR),
        })
    }
}

const POWER_OVERLAY_TITLE: &str = "ChickenWM Power";

pub(crate) fn prepare_hwnd_no_transitions(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::Graphics::Dwm::{
        DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE,
        DWMWA_TRANSITIONS_FORCEDISABLED,
    };
    use windows::Win32::UI::Controls::MARGINS;
    unsafe {
        if hwnd.0.is_null() {
            return;
        }
        chickenwm_core::sys::suppress_nc_paint(hwnd);
        let disable = windows::Win32::Foundation::BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &disable as *const _ as *const core::ffi::c_void,
            size_of::<windows::Win32::Foundation::BOOL>() as u32,
        );
        // No Windows 11 frame line, system rounding, or full-rect acrylic sheet over transparent areas.
        let no_border: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE
        let _ = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_BORDER_COLOR,
            &no_border as *const _ as *const core::ffi::c_void,
            size_of::<u32>() as u32,
        );
        let no_round = windows::Win32::Graphics::Dwm::DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE,
            &no_round as *const _ as *const core::ffi::c_void,
            size_of::<windows::Win32::Graphics::Dwm::DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let no_backdrop: u32 = 1; // DWMSBT_NONE
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &no_backdrop as *const _ as *const core::ffi::c_void,
            size_of::<u32>() as u32,
        );
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
}

/// Finds a window by title, disables its DWM open/close animation and returns whether it was found.
pub(crate) fn prepare_window_no_transitions(title: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    unsafe {
        let Ok(hwnd) = FindWindowW(None, &windows::core::HSTRING::from(title)) else {
            return false;
        };
        if hwnd.0.is_null() {
            return false;
        }
        prepare_hwnd_no_transitions(hwnd);
        true
    }
}

fn prepare_power_overlay_window() -> bool {
    prepare_window_no_transitions(POWER_OVERLAY_TITLE)
}

struct SharedBarState {
    snapshot: BarStateSnapshot,
    css: CssStyleSheet,
    egui_ctx: Option<egui::Context>,
}

struct RooBarApp {
    shared: Arc<Mutex<SharedBarState>>,
    /// The bar window.
    bar_hwnd: Option<isize>,
    /// `--font-family` value last applied, so the font is only reloaded when the CSS changes it.
    applied_font_family: Option<String>,
    /// The loaded font is a Nerd Font, so icon glyphs can be used.
    nerd_icons: bool,
    last_title: String,
    title_changed: Instant,
    cpu_tracker: CpuTracker,
    mem_used_gb: f32,
    mem_total_gb: f32,
    mem_pct: u32,
    /// Network, volume and Bluetooth, polled on a background thread.
    status: StatusMonitor,
    last_sys_poll: Instant,
    last_wm_poll: Instant,
    power_overlay: Option<PowerOverlay>,
    /// A hamburger-menu pick waiting for the menu to finish closing.
    pending_menu: Option<popover::MenuAction>,
    /// Calendar / volume / processes / media / network drop-down.
    popover: Option<Popover>,
    /// The popover that last closed and when: clicking its widget closes it (the click takes
    /// focus first), and must not reopen it straight away.
    popover_closed: Option<(PopoverKind, Instant)>,
    /// Tray icon textures by icon handle.
    tray_textures: std::collections::HashMap<u64, Option<egui::TextureHandle>>,
    /// Open window icon textures by HWND for workspace buttons.
    window_icon_textures: std::collections::HashMap<u64, Option<egui::TextureHandle>>,
    /// User-defined tray icon order (`(hwnd, uid)`).
    tray_order: Vec<(u64, u32)>,
    /// User-hidden tray icons (`(hwnd, uid)`), shown dimmed only when the tray is expanded.
    tray_hidden: std::collections::HashSet<(u64, u32)>,
    /// Currently dragged tray icon (`(hwnd, uid)`).
    tray_dragging: Option<(u64, u32)>,
    /// The tray shows every icon (the overflow chevron was clicked).
    tray_expanded: bool,
    /// Live window thumbnails when hovering a workspace pill.
    previews: previews::Previews,
    hover_cards: hovercard::HoverCards,
    /// Monitor the bar lives on, in the same units the viewports are positioned in.
    screen_rect: egui::Rect,
    last_float: Option<bool>,
}

impl RooBarApp {
    fn new(shared: Arc<Mutex<SharedBarState>>, screen_rect: egui::Rect) -> Self {
        let mut cpu_tracker = CpuTracker::default();
        let _ = cpu_tracker.sample();
        let (mem_used_gb, mem_total_gb, mem_pct) = query_memory_stats();
        let repaint = Arc::clone(&shared);
        let status = StatusMonitor::spawn(move || {
            if let Some(ctx) = repaint.lock().egui_ctx.as_ref() {
                ctx.request_repaint();
            }
        });
        Self {
            shared,
            applied_font_family: None,
            nerd_icons: false,
            last_title: String::new(),
            title_changed: Instant::now(),
            cpu_tracker,
            mem_used_gb,
            mem_total_gb,
            mem_pct,
            status,
            last_sys_poll: Instant::now(),
            last_wm_poll: Instant::now(),
            power_overlay: None,
            pending_menu: None,
            popover: None,
            popover_closed: None,
            last_float: None,
            tray_textures: Default::default(),
            window_icon_textures: Default::default(),
            tray_order: Vec::new(),
            tray_hidden: Default::default(),
            tray_dragging: None,
            tray_expanded: false,
            previews: Default::default(),
            hover_cards: Default::default(),
            bar_hwnd: None,
            screen_rect,
        }
    }

    /// Loads the font named by `--font-family` (first installed match) as the bar's font.
    fn apply_font(&mut self, ctx: &egui::Context, css: &CssStyleSheet) {
        let families = css
            .custom_properties
            .get("--font-family")
            .cloned()
            .unwrap_or_default();
        if self.applied_font_family.as_deref() == Some(families.as_str()) {
            return;
        }
        self.applied_font_family = Some(families.clone());
        self.nerd_icons = false;

        if let Some(path) = chickenwm_core::fonts::apply_egui_font(ctx, &families, true) {
            self.nerd_icons = path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().contains("nerd"));
            info!("roobar font: {}", path.display());
        }
    }

    /// Nerd Font glyph when available, otherwise a plain fallback symbol.
    #[allow(dead_code)]
    fn icon(&self, nerd: &str, fallback: &str) -> String {
        if self.nerd_icons { nerd } else { fallback }.to_owned()
    }

    fn dispatch_wm_action(&self, action: Action) {
        // Optimistic immediate UI update for workspace switching so the indicator responds in 0ms
        if let Action::Workspace(target_ws) = action {
            let target_ws = target_ws.clamp(1, 10);
            let mut lock = self.shared.lock();
            lock.snapshot.active_workspace = target_ws;
            for ws in &mut lock.snapshot.workspaces {
                ws.active = ws.id == target_ws;
            }
        } else if let Action::WorkspaceRelative(delta) = action {
            let mut lock = self.shared.lock();
            let cur = lock.snapshot.active_workspace as i32;
            let next = ((cur - 1 + delta).rem_euclid(10) + 1) as u32;
            lock.snapshot.active_workspace = next;
            for ws in &mut lock.snapshot.workspaces {
                ws.active = ws.id == next;
            }
        }

        std::thread::spawn(move || {
            if send_ipc_command_blocking(PIPE_NAME_WM, &IpcCommand::Dispatch(action.clone()))
                .is_err()
            {
                let _ = send_ipc_command_blocking(PIPE_NAME_DAEMON, &IpcCommand::Dispatch(action));
            }
        });
    }
}

/// What every widget draws with, resolved once per frame.
struct WidgetEnv<'a> {
    style: &'a IslandStyle,
    font: egui::FontId,
    fg: egui::Color32,
    subtext: egui::Color32,
    island_h: f32,
    snap: &'a BarStateSnapshot,
    css: &'a CssStyleSheet,
}

impl RooBarApp {
    /// Opens `kind` under `anchor`, or closes it if it is already open.
    fn toggle_popover(&mut self, ctx: &egui::Context, kind: PopoverKind, anchor: egui::Rect) {
        if let Some(p) = self.popover.as_mut() {
            if p.kind == kind {
                p.begin_close();
                return;
            }
        }
        // This click is what just closed it (by taking focus away): leave it closed.
        if self
            .popover_closed
            .is_some_and(|(k, at)| k == kind && at.elapsed() < Duration::from_millis(350))
        {
            return;
        }
        self.popover = Some(Popover::new(ctx, kind, anchor));
    }

    /// Draws one library widget. `slot` keeps ids unique when a widget is listed twice;
    /// `in_group` draws it as a segment of a shared pill (`--modules-group`).
    fn draw_widget(&mut self, ui: &mut egui::Ui, widget: Widget, slot: usize, in_group: bool, env: &WidgetEnv) {
        let id = egui::Id::new(("widget", widget.name(), slot));
        match widget {
            Widget::Stats => {
                let t1 = format!("{:>2.0}%", self.cpu_tracker.usage_pct);
                let t2 = format!("{:.1}G", self.mem_used_gb);
                let g1 = ui.painter().layout_no_wrap(t1, env.font.clone(), env.fg);
                let g2 = ui.painter().layout_no_wrap(t2, env.font.clone(), env.fg);
                let icon_w = 14.0_f32;
                let gap = 5.0_f32;
                let mid_gap = 10.0_f32;
                let bold_extra = if env.style.bold { 1.0 } else { 0.0 };
                let content_w = icon_w + gap + g1.size().x + mid_gap + icon_w + gap + g2.size().x + bold_extra * 2.0;
                let padding = if in_group { 16.0 } else { 22.0 };
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(content_w + padding, env.island_h), egui::Sense::click());
                let hover_t = ui.ctx().animate_bool_with_time(id, resp.hovered(), 0.15);
                if in_group {
                    paint_hover_highlight(ui.painter(), rect, env.style, hover_t);
                } else {
                    paint_island(ui.painter(), rect, env.style, hover_t);
                }
                let mut cur_x = rect.center().x - content_w / 2.0;
                let icon1_r = egui::Rect::from_center_size(egui::pos2(cur_x + icon_w / 2.0, rect.center().y), egui::vec2(icon_w, icon_w));
                chickenwm_core::ui_icons::paint_cpu(ui.painter(), icon1_r, env.fg);
                cur_x += icon_w + gap;
                let g1_w = g1.size().x;
                draw_text(ui.painter(), egui::pos2(cur_x, rect.center().y - g1.size().y / 2.0), g1, env.fg, env.style.bold);
                cur_x += g1_w + bold_extra + mid_gap;
                let icon2_r = egui::Rect::from_center_size(egui::pos2(cur_x + icon_w / 2.0, rect.center().y), egui::vec2(icon_w, icon_w));
                chickenwm_core::ui_icons::paint_memory(ui.painter(), icon2_r, env.fg);
                cur_x += icon_w + gap;
                draw_text(ui.painter(), egui::pos2(cur_x, rect.center().y - g2.size().y / 2.0), g2, env.fg, env.style.bold);
                let resp = resp.hover_card(format!(
                    "RAM {:.1} / {:.1} GB ({}%)\nClick for top processes",
                    self.mem_used_gb, self.mem_total_gb, self.mem_pct
                ));
                if gestures(ui, env.css, widget, &resp).click {
                    self.toggle_popover(ui.ctx(), PopoverKind::Processes, resp.rect);
                }
            }
            Widget::Cpu => {
                let text = format!("{:>2.0}%", self.cpu_tracker.usage_pct);
                let resp = pill_vector(
                    ui, id, env.style, Some(&text), env.font.clone(), env.fg, env.island_h, true, in_group,
                    Some(chickenwm_core::ui_icons::paint_cpu),
                ).hover_card("Click for top processes");
                if gestures(ui, env.css, widget, &resp).click {
                    self.toggle_popover(ui.ctx(), PopoverKind::Processes, resp.rect);
                }
            }
            Widget::Memory => {
                let text = format!("{:.1}G", self.mem_used_gb);
                let resp = pill_vector(
                    ui, id, env.style, Some(&text), env.font.clone(), env.fg, env.island_h, true, in_group,
                    Some(chickenwm_core::ui_icons::paint_memory),
                ).hover_card(format!(
                    "RAM {:.1} / {:.1} GB ({}%)\nClick for top processes",
                    self.mem_used_gb, self.mem_total_gb, self.mem_pct
                ));
                if gestures(ui, env.css, widget, &resp).click {
                    self.toggle_popover(ui.ctx(), PopoverKind::Processes, resp.rect);
                }
            }
            Widget::Notifications => {
                let s = self.status.snapshot();
                let text = match (s.dnd, s.unread) {
                    (true, 0) => Some("DND".to_string()),
                    (true, n) => Some(format!("DND ({n})")),
                    (false, 0) => None,
                    (false, n) => Some(format!("{n}")),
                };
                let color = if s.dnd || s.unread > 0 {
                    env.fg
                } else {
                    env.subtext
                };
                let hover = match (s.unread, s.dnd) {
                    (0, false) => "No new notifications".to_owned(),
                    (n, false) => format!("{n} new notification(s)"),
                    (n, true) => format!("Do not disturb is on ({n} new)"),
                };
                let dnd = s.dnd;
                let resp = pill_vector(
                    ui, id, env.style, text.as_deref(), env.font.clone(), color, env.island_h, true, in_group,
                    Some(move |p: &egui::Painter, r: egui::Rect, c: egui::Color32| {
                        chickenwm_core::ui_icons::paint_bell(p, r, c, dnd);
                    }),
                ).hover_card(format!("{hover}\nClick for the notification center (Super + N)"));
                if gestures(ui, env.css, widget, &resp).click {
                    std::thread::spawn(|| {
                        let _ = send_ipc_command_blocking(chickenwm_core::ipc::PIPE_NAME_NOTIFY, &IpcCommand::ToggleNotificationCenter);
                    });
                }
            }
            Widget::Tray => {
                let mut icons = self.status.snapshot().tray;
                if icons.is_empty() {
                    return;
                }
                // Sync user-defined tray_order with current live icons
                let live_keys: std::collections::HashSet<(u64, u32)> =
                    icons.iter().map(|i| (i.hwnd, i.uid)).collect();
                self.tray_order.retain(|k| live_keys.contains(k));
                for icon in &icons {
                    let key = (icon.hwnd, icon.uid);
                    if !self.tray_order.contains(&key) {
                        self.tray_order.push(key);
                    }
                }
                icons.sort_by_key(|i| {
                    self.tray_order
                        .iter()
                        .position(|&k| k == (i.hwnd, i.uid))
                        .unwrap_or(usize::MAX)
                });

                let (unhidden, hidden_icons): (Vec<_>, Vec<_>) = icons
                    .into_iter()
                    .partition(|i| !self.tray_hidden.contains(&(i.hwnd, i.uid)));
                let visible = env.css.var_px("--tray-visible", 4.0).max(1.0) as usize;
                let overflow = unhidden.len() > visible || !hidden_icons.is_empty();
                let mut display_icons: Vec<(chickenwm_core::ipc::TrayIcon, bool)> = if self.tray_expanded {
                    let mut v: Vec<_> = unhidden.into_iter().map(|i| (i, false)).collect();
                    v.extend(hidden_icons.into_iter().map(|i| (i, true)));
                    v
                } else {
                    unhidden
                        .into_iter()
                        .take(visible)
                        .map(|i| (i, false))
                        .collect()
                };
                if display_icons.is_empty() && !overflow {
                    return;
                }
                let shown = display_icons.len();
                let (size, gap, pad, chevron) = (18.0, 7.0, 11.0, 14.0);
                let icons_w = if shown > 0 {
                    shown as f32 * (size + gap) - gap
                } else {
                    0.0
                };
                let w = pad * 2.0 + icons_w + if overflow { (if shown > 0 { gap } else { 0.0 }) + chevron } else { 0.0 };
                let (rect, _) = ui.allocate_exact_size(egui::vec2(w, env.island_h), egui::Sense::hover());
                paint_island(ui.painter(), rect, env.style, 0.0);
                // Textures for icons not seen before (the cache is pruned to the current set).
                let live: std::collections::HashSet<u64> = display_icons.iter().map(|(i, _)| i.icon).collect();
                self.tray_textures.retain(|k, _| live.contains(k));
                let pointer_pos = ui.input(|i| i.pointer.interact_pos());
                let mut swap_pair: Option<((u64, u32), (u64, u32))> = None;

                for (i, (icon, is_hidden)) in display_icons.drain(..).enumerate() {
                    let key = (icon.hwnd, icon.uid);
                    let r = egui::Rect::from_center_size(
                        egui::pos2(rect.left() + pad + size / 2.0 + i as f32 * (size + gap), rect.center().y),
                        egui::vec2(size, size),
                    );
                    let tex = self.tray_textures.entry(icon.icon).or_insert_with(|| {
                        tray::icon_rgba(icon.icon).map(|(w, h, px)| {
                            ui.ctx().load_texture(
                                format!("tray-{}", icon.icon),
                                egui::ColorImage::from_rgba_unmultiplied([w, h], &px),
                                egui::TextureOptions::LINEAR,
                            )
                        })
                    });
                    let resp = ui.interact(r.expand(3.0), id.with(key), egui::Sense::click_and_drag());
                    if resp.drag_started() {
                        self.tray_dragging = Some(key);
                    }
                    if let Some(drag_key) = self.tray_dragging {
                        if drag_key != key && pointer_pos.is_some_and(|p| r.expand(3.0).contains(p)) {
                            swap_pair = Some((drag_key, key));
                        }
                    }
                    if resp.drag_stopped() {
                        self.tray_dragging = None;
                    }
                    if resp.hovered() || self.tray_dragging == Some(key) {
                        ui.painter().rect_filled(r.expand(3.0), 4.0, env.fg.gamma_multiply(0.14));
                    }
                    let tint = if is_hidden {
                        egui::Color32::from_white_alpha(105)
                    } else {
                        egui::Color32::WHITE
                    };
                    match tex {
                        Some(t) => {
                            ui.painter().image(
                                t.id(),
                                r,
                                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                tint,
                            );
                        }
                        None => {
                            ui.painter().circle_filled(
                                r.center(),
                                size / 3.0,
                                if is_hidden { env.subtext.gamma_multiply(0.45) } else { env.subtext },
                            );
                        }
                    }
                    let tip_base = if icon.tip.is_empty() { "Tray icon" } else { &icon.tip };
                    let tip = if is_hidden {
                        format!("{tip_base} (hidden)\nMiddle-click to unhide • Drag to reorder")
                    } else {
                        format!("{tip_base}\nMiddle-click to hide • Drag to reorder")
                    };
                    let resp = resp.hover_card(tip);
                    if resp.middle_clicked() {
                        if !self.tray_hidden.remove(&key) {
                            self.tray_hidden.insert(key);
                        }
                    } else {
                        let click = if resp.double_clicked() {
                            Some(tray::Click::Double)
                        } else if resp.clicked() {
                            Some(tray::Click::Left)
                        } else if resp.secondary_clicked() {
                            Some(tray::Click::Right)
                        } else {
                            None
                        };
                        if let Some(click) = click {
                            let mut pt = windows::Win32::Foundation::POINT::default();
                            unsafe {
                                let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
                            }
                            tray::send_click(&icon, click, (pt.x, pt.y));
                        }
                    }
                }
                if let Some((a, b)) = swap_pair {
                    if let (Some(ia), Some(ib)) = (
                        self.tray_order.iter().position(|&k| k == a),
                        self.tray_order.iter().position(|&k| k == b),
                    ) {
                        self.tray_order.swap(ia, ib);
                    }
                }
                if overflow {
                    let c = egui::Rect::from_center_size(egui::pos2(rect.right() - pad - chevron / 2.0, rect.center().y), egui::vec2(chevron, size));
                    let resp = ui.interact(c, id.with("chevron"), egui::Sense::click());
                    let glyph = if self.tray_expanded { "›" } else { "‹" };
                    let color = if resp.hovered() { env.fg } else { env.subtext };
                    ui.painter().text(c.center(), egui::Align2::CENTER_CENTER, glyph, env.font.clone(), color);
                    let total_icons = self.tray_order.len();
                    let hidden_count = total_icons.saturating_sub(shown);
                    let resp = resp.hover_card(if self.tray_expanded {
                        "Collapse tray".to_owned()
                    } else {
                        format!("{hidden_count} more (including hidden)")
                    });
                    if resp.clicked() {
                        self.tray_expanded = !self.tray_expanded;
                    }
                }
            }
            Widget::Media => {
                let Some(m) = self.status.snapshot().media else {
                    return;
                };
                let is_playing = m.playing;
                let mut text: String = if m.artist.is_empty() {
                    m.title.clone()
                } else {
                    format!("{} — {}", m.title, m.artist)
                };
                let max = env.css.var_px("--media-max-chars", 32.0).max(8.0) as usize;
                if text.chars().count() > max {
                    text = text.chars().take(max - 1).collect::<String>() + "…";
                }
                let color = if is_playing { env.fg } else { env.subtext };
                let resp = pill_vector(
                    ui, id, env.style, Some(&text), env.font.clone(), color, env.island_h, true, in_group,
                    Some(move |p: &egui::Painter, r: egui::Rect, c: egui::Color32| {
                        if is_playing {
                            chickenwm_core::ui_icons::paint_media_play(p, r, c);
                        } else {
                            chickenwm_core::ui_icons::paint_media_pause(p, r, c);
                        }
                    }),
                ).hover_card("Click for controls • Middle-click to play/pause");
                let g = gestures(ui, env.css, widget, &resp);
                if g.click {
                    self.toggle_popover(ui.ctx(), PopoverKind::Media, resp.rect);
                }
                if g.middle {
                    self.status.send(Cmd::PlayPause);
                }
            }
            Widget::RefreshRate => {
                let text = format!("{}Hz", env.snap.monitor_refresh_hz);
                pill_vector(
                    ui, id, env.style, Some(&text), env.font.clone(), env.subtext, env.island_h, false, in_group,
                    Some(chickenwm_core::ui_icons::paint_displays),
                );
            }
            Widget::Network => {
                let show_label = widgets::css_flag(env.css, "--network-label", false);
                let net = self.status.snapshot().network;
                let (name, hover, color) = match &net {
                    status::Network::Ethernet {
                        adapter,
                        speed_mbps,
                        ip,
                    } => (
                        "ethernet".to_owned(),
                        format!(
                            "Ethernet ({adapter})\nSpeed: {speed_mbps} Mbps • IP: {}",
                            if ip.is_empty() { "—" } else { &ip }
                        ),
                        env.fg,
                    ),
                    status::Network::Wifi {
                        ssid,
                        signal,
                        speed_mbps,
                        ip,
                    } => (
                        ssid.clone(),
                        format!(
                            "Wi-Fi: {} ({signal}%)\nSpeed: {speed_mbps} Mbps • IP: {}",
                            if ssid.is_empty() { "connected" } else { &ssid },
                            if ip.is_empty() { "—" } else { &ip }
                        ),
                        env.fg,
                    ),
                    status::Network::Disconnected => (
                        "offline".to_owned(),
                        "Not connected".to_owned(),
                        env.subtext,
                    ),
                };
                let text = if show_label && !name.is_empty() { Some(name.as_str()) } else { None };
                let resp = pill_vector(
                    ui, id, env.style, text, env.font.clone(), color, env.island_h, true, in_group,
                    Some(move |p: &egui::Painter, r: egui::Rect, c: egui::Color32| {
                        match net {
                            status::Network::Ethernet { .. } => chickenwm_core::ui_icons::paint_ethernet(p, r, c),
                            status::Network::Wifi { signal, .. } => chickenwm_core::ui_icons::paint_wifi(p, r, c, signal as u8),
                            status::Network::Disconnected => chickenwm_core::ui_icons::paint_wifi(p, r, c, 0),
                        }
                    }),
                ).hover_card(format!("{hover}\nClick for Wi-Fi networks • Right-click for network settings"));
                let g = gestures(ui, env.css, widget, &resp);
                if g.click {
                    self.toggle_popover(ui.ctx(), PopoverKind::Network, resp.rect);
                }
                if g.right {
                    status::open_uri("ms-settings:network-status");
                }
            }
            Widget::Volume => {
                let Some(vol) = self.status.snapshot().volume else {
                    return;
                };
                let pct = (vol.level * 100.0).round() as u32;
                let muted = vol.muted;
                let (text, color) = if muted {
                    ("muted".to_owned(), env.subtext)
                } else {
                    (format!("{pct}%"), env.fg)
                };
                let resp = pill_vector(
                    ui, id, env.style, Some(&text), env.font.clone(), color, env.island_h, true, in_group,
                    Some(move |p: &egui::Painter, r: egui::Rect, c: egui::Color32| {
                        chickenwm_core::ui_icons::paint_volume(p, r, c, muted, pct);
                    }),
                ).hover_card("Click for volume and output • Middle-click to mute • Scroll to change • Right-click for sound settings");
                let g = gestures(ui, env.css, widget, &resp);
                if g.click {
                    self.toggle_popover(ui.ctx(), PopoverKind::Volume, resp.rect);
                }
                if g.middle {
                    self.status.send(Cmd::ToggleMute);
                }
                if g.right {
                    status::open_uri("ms-settings:sound");
                }
                {
                    let scroll = g.scroll;
                    if scroll != 0.0 {
                        let step = env.css.var_px("--volume-step", 5.0).clamp(1.0, 25.0) / 100.0;
                        self.status.send(Cmd::Adjust(if scroll > 0.0 { step } else { -step }));
                    }
                }
            }
            Widget::Bluetooth => {
                let (text, hover, color) = match self.status.snapshot().bluetooth {
                    status::Bluetooth::Unavailable => return,
                    status::Bluetooth::Off => {
                        (None, "Bluetooth is off".to_owned(), env.subtext)
                    }
                    status::Bluetooth::On { devices } if devices.is_empty() => {
                        (None, "Bluetooth on, nothing connected".to_owned(), env.fg)
                    }
                    status::Bluetooth::On { devices } => {
                        let label = if !widgets::css_flag(env.css, "--bluetooth-label", false) {
                            None
                        } else if devices.len() == 1 {
                            let name: String = devices[0].chars().take(18).collect();
                            Some(name)
                        } else {
                            Some(format!("{}", devices.len()))
                        };
                        (label, format!("Connected: {}", devices.join(", ")), env.fg)
                    }
                };
                let resp = pill_vector(
                    ui, id, env.style, text.as_deref(), env.font.clone(), color, env.island_h, true, in_group,
                    Some(chickenwm_core::ui_icons::paint_bluetooth),
                ).hover_card(format!("{hover}\nClick for Bluetooth settings"));
                if gestures(ui, env.css, widget, &resp).click {
                    status::open_uri("ms-settings:bluetooth");
                }
            }
            Widget::Battery => {
                let Some(bat) = self.status.snapshot().battery else {
                    return; // Guard: Desktop PC or VM with no battery — completely omitted!
                };
                let pct = bat.percent;
                let charging = bat.charging;
                let text = format!("{pct}%");
                let color = if charging {
                    env.fg
                } else if pct <= 15 {
                    egui::Color32::from_rgb(235, 95, 95)
                } else {
                    env.fg
                };
                let tooltip = match (charging, bat.remaining_seconds) {
                    (true, _) => format!("Battery: {pct}% (Charging)"),
                    (false, Some(sec)) => {
                        let mins = (sec / 60) % 60;
                        let hrs = sec / 3600;
                        format!("Battery: {pct}%\n{hrs}h {mins}m remaining")
                    }
                    (false, None) => format!("Battery: {pct}%"),
                };
                let resp = pill_vector(
                    ui, id, env.style, Some(&text), env.font.clone(), color, env.island_h, true, in_group,
                    Some(move |p: &egui::Painter, r: egui::Rect, c: egui::Color32| {
                        chickenwm_core::ui_icons::paint_battery(p, r, c, pct as u32, charging);
                    }),
                ).hover_card(tooltip);
                if gestures(ui, env.css, widget, &resp).click {
                    status::open_uri("ms-settings:batterysaver");
                }
            }
            Widget::Files => {
                let resp = pill_vector(
                    ui, id, env.style, None, env.font.clone(), env.fg, env.island_h, true, in_group,
                    Some(chickenwm_core::ui_icons::paint_folder),
                ).hover_card("Open ChickenWM File Explorer (Super + E)");
                if gestures(ui, env.css, widget, &resp).click {
                    spawn_sibling("chickenwm-files.exe");
                }
            }
            Widget::Settings => {
                let resp = pill_vector(
                    ui, id, env.style, None, env.font.clone(), env.fg, env.island_h, true, in_group,
                    Some(chickenwm_core::ui_icons::paint_gear),
                ).hover_card("Open ChickenWM Settings (Super + I)");
                if gestures(ui, env.css, widget, &resp).click {
                    spawn_sibling("chickenwm-settings.exe");
                }
            }
            Widget::Clock => {
                let fmt = widgets::css_string(env.css, "--clock-format")
                    .unwrap_or_else(|| widgets::DEFAULT_CLOCK_FORMAT.to_owned());
                let text = format_local_clock(&fmt);
                let resp = pill_vector(
                    ui, id, env.style, Some(&text), env.font.clone(), env.fg, env.island_h, true, in_group,
                    Some(chickenwm_core::ui_icons::paint_clock),
                ).hover_card("Click for the calendar");
                if gestures(ui, env.css, widget, &resp).click {
                    self.toggle_popover(ui.ctx(), PopoverKind::Calendar, resp.rect);
                }
            }
            Widget::Power => {
                let island_h = env.island_h;
                let (power_rect, power_resp) =
                    ui.allocate_exact_size(egui::vec2(island_h + 6.0, island_h), egui::Sense::click());
                let hover_t =
                    ui.ctx().animate_bool_with_time(id, power_resp.hovered() || self.power_overlay.is_some(), 0.15);
                paint_island(ui.painter(), power_rect, env.style, hover_t);
                paint_power_glyph(ui.painter(), power_rect.center(), island_h * 0.24, env.fg);
                let power_resp = power_resp.hover_card("Shut down / Restart / Log out");
                if gestures(ui, env.css, widget, &power_resp).click && self.power_overlay.is_none() {
                    // Snapshot the monitor now, before the overlay window covers it.
                    let backdrop = Backdrop::capture(ui.ctx(), self.screen_rect);
                    self.power_overlay = Some(PowerOverlay { opened: Instant::now(), closing: None, shown: false, backdrop });
                }
            }
            Widget::Menu => {
                let island_h = env.island_h;
                let menu_open = self.popover.as_ref().is_some_and(|p| matches!(p.kind, PopoverKind::Menu | PopoverKind::About));
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(island_h + 6.0, island_h), egui::Sense::click());
                let hover_t = ui.ctx().animate_bool_with_time(id, resp.hovered() || menu_open, 0.15);
                paint_island(ui.painter(), rect, env.style, hover_t);
                paint_menu_glyph(ui.painter(), rect.center(), island_h * 0.24, env.fg);
                let resp = resp.hover_card("File Explorer, Settings, About, Shut down");
                if gestures(ui, env.css, widget, &resp).click {
                    if self.popover.as_ref().is_some_and(|p| p.kind == PopoverKind::About) {
                        if let Some(p) = self.popover.as_mut() {
                            p.begin_close();
                        }
                    } else {
                        self.toggle_popover(ui.ctx(), PopoverKind::Menu, resp.rect);
                    }
                }
            }
        }
    }
}

impl eframe::App for RooBarApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.hover_cards.begin_frame();
        let bar_outer = ctx.input(|i| i.viewport().outer_rect).unwrap_or(ctx.screen_rect());
        hovercard::set_origin(bar_outer.min);
        if self.bar_hwnd.is_none() {
            if let Ok(h) = frame.window_handle() {
                if let RawWindowHandle::Win32(w) = h.as_raw() {
                    let hwnd = windows::Win32::Foundation::HWND(w.hwnd.get() as *mut _);
                    self.bar_hwnd = Some(w.hwnd.get());
                    prepare_hwnd_no_transitions(hwnd);
                }
            }
        }
        {
            let mut lock = self.shared.lock();
            if lock.egui_ctx.is_none() {
                lock.egui_ctx = Some(ctx.clone());
            }
        }

        // Poll system telemetry & clock once per second
        if self.last_sys_poll.elapsed() >= Duration::from_millis(950) {
            self.last_sys_poll = Instant::now();
            self.cpu_tracker.sample();
            let (u, t, p) = query_memory_stats();
            self.mem_used_gb = u;
            self.mem_total_gb = t;
            self.mem_pct = p;
        }

        // Periodic fallback sync with `chickenwm-wm` (in addition to instant push over `PIPE_NAME_BAR`)
        if self.last_wm_poll.elapsed() >= Duration::from_millis(300) {
            self.last_wm_poll = Instant::now();
            let shared_clone = Arc::clone(&self.shared);
            std::thread::spawn(move || {
                if let Ok(IpcResponse::BarState(snap)) =
                    send_ipc_command_blocking(PIPE_NAME_WM, &IpcCommand::QueryBarState)
                {
                    let mut lock = shared_clone.lock();
                    if lock.snapshot != snap {
                        lock.snapshot = snap;
                        if let Some(ref c) = lock.egui_ctx {
                            c.request_repaint();
                        }
                    }
                }
            });
        }

        let (snap, css) = {
            let lock = self.shared.lock();
            (lock.snapshot.clone(), lock.css.clone())
        };

        self.apply_font(ctx, &css);

        // Palette from `~/.config/chickenwm/roobar.css` (defaults: sepia islands, dark-brown text).
        let rgba = |r, g, b, a| RgbaColor { r, g, b, a };
        let col = |name: &str, fallback: RgbaColor| to_color32(css.var_color(name, fallback));
        let bar_bg = col("--bar-bg", rgba(0, 0, 0, 0));
        let island_bg = col("--pill-bg", rgba(240, 230, 211, 242));
        let fg_color = col("--fg-color", rgba(43, 29, 20, 255));
        let subtext_color = col("--subtext-color", rgba(107, 85, 69, 255));
        let accent_color = col("--accent-color", rgba(245, 236, 220, 255));
        let accent_secondary = col("--accent-secondary", rgba(138, 106, 80, 255));
        let ws_active_bg = col("--ws-active-bg", rgba(59, 42, 31, 255));
        let ws_active_fg = col("--ws-active-fg", rgba(245, 236, 220, 255));
        let ws_occupied_bg = col("--ws-occupied-bg", rgba(59, 42, 31, 36));
        let ws_occupied_fg = col("--ws-occupied-fg", rgba(43, 29, 20, 255));
        let ws_empty_fg = col("--ws-empty-fg", rgba(43, 29, 20, 115));
        let border_color = col("--border-color", rgba(59, 42, 31, 70));
        let border_radius = css.var_px("--border-radius", 12.0);
        let pill_radius = css.var_px("--pill-radius", 10.0);
        let font_size = snap_font_size(
            css.var_px("--font-size", 22.0),
            css.var_px("--font-pixel-grid", 0.0),
            ctx.pixels_per_point(),
        );
        let bold = css.custom_properties.get("--font-weight").is_some_and(|w| {
            let w = w.trim();
            w.eq_ignore_ascii_case("bold") || w.parse::<u32>().is_ok_and(|n| n >= 600)
        });
        let island_gap = css.var_px("--island-gap", 8.0);
        let anim_secs = (css.var_px("--anim-ms", 220.0) / 1000.0).max(0.01);
        let font = egui::FontId::proportional(font_size);

        let is_float = widgets::css_flag(&css, "--float", true);
        if self.last_float != Some(is_float) {
            self.last_float = Some(is_float);
            let bar_height = css.var_px("--bar-height", 36.0);
            let margin_top = if is_float { css.var_px("--bar-margin-top", 6.0) } else { 0.0 };
            let margin_x = if is_float { css.var_px("--bar-margin-x", 10.0) } else { 0.0 };
            let bar_x = if is_float { self.screen_rect.min.x + margin_x } else { self.screen_rect.min.x };
            let bar_y = if is_float { self.screen_rect.min.y + margin_top } else { self.screen_rect.min.y };
            let bar_w = if is_float { (self.screen_rect.width() - margin_x * 2.0).max(400.0) } else { self.screen_rect.width() };
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(bar_w, bar_height)));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(bar_x, bar_y)));
        }

        let islands_enabled = is_float && widgets::css_flag(&css, "--islands", true);
        let style = IslandStyle {
            bg: island_bg,
            hover_bg: island_bg.lerp_to_gamma(accent_color, 0.55),
            border: border_color,
            radius: if is_float { pill_radius } else { 0.0 },
            bold,
            flat: !islands_enabled,
        };
        // Window title: cross-fade + ease the island width whenever the text changes.
        let title_text = if snap.active_window_title.is_empty() {
            format!("desktop · workspace {}", snap.active_workspace)
        } else {
            let title: String = snap.active_window_title.chars().take(64).collect();
            let process = snap
                .active_window_process
                .trim_end_matches(".exe")
                .trim_end_matches(".EXE");
            if process.is_empty() {
                title
            } else {
                format!("{process} · {title}")
            }
        };
        if title_text != self.last_title {
            self.last_title = title_text.clone();
            self.title_changed = Instant::now();
        }
        let title_fade = (self.title_changed.elapsed().as_secs_f32() / anim_secs).clamp(0.0, 1.0);
        if title_fade < 1.0 {
            ctx.request_repaint();
        }

        // Without islands the bar itself is the one pill: the island fill unless --bar-bg is set.
        let bar_bg = if style.flat && bar_bg.a() == 0 { island_bg } else { bar_bg };
        let border_radius = if !is_float {
            0.0
        } else if style.flat {
            border_radius.max(pill_radius)
        } else {
            border_radius
        };
        let bar_frame = egui::Frame::NONE
            .fill(bar_bg)
            .stroke(if is_float && bar_bg.a() > 0 {
                egui::Stroke::new(1.0_f32, border_color)
            } else {
                egui::Stroke::NONE
            })
            .corner_radius(border_radius)
            // Room around the islands for their drop shadows.
            .inner_margin(if is_float {
                egui::Margin::symmetric(6, 5)
            } else {
                egui::Margin::symmetric(10, 3)
            });

        // The workspace pill under the pointer (for the window previews).
        let mut hovered_ws: Option<(u32, egui::Rect)> = None;
        egui::CentralPanel::default()
            .frame(bar_frame)
            .show(ctx, |ui| {
                let island_h = ui.available_height();
                {
                    let full = ui.max_rect();
                    let mut left_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(full)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    // =========================================================================
                    // LEFT: launcher · workspaces · layout
                    // =========================================================================
                    left_ui.horizontal_centered(|ui| {
                        ui.spacing_mut().item_spacing.x = island_gap;

                        if island_vector(
                            ui,
                            egui::Id::new("island-launcher"),
                            &style,
                            Some("chickenwm"),
                            font.clone(),
                            fg_color,
                            island_h,
                            true,
                            Some(chickenwm_core::ui_icons::paint_launcher),
                        )
                        .hover_card("Summon ChickenWM Launcher (Super + D)")
                        .clicked()
                        {
                            std::thread::spawn(|| {
                                let _ = send_ipc_command_blocking(
                                    PIPE_NAME_DAEMON,
                                    &IpcCommand::SummonLauncher,
                                );
                            });
                        }

                        // #workspaces island: occupied workspaces show as pills with open program icons,
                        // active empty workspace shows as a sleek pill, and inactive empty workspaces show as dots.
                        let pill_h = (island_h - 10.0).clamp(16.0, 22.0);
                        let pill_gap = 4.0;
                        let inner_pad = 8.0;
                        const MAX_WS_ICONS: usize = 6;
                        let icon_size = (pill_h - 4.0).clamp(13.0, 16.0);
                        let icon_gap = 4.0;
                        let pill_pad_x = 8.0;
                        let empty_active_w = (pill_h * 1.65).round();
                        let empty_dot_w = 12.0;

                        // Prune cached window icon textures to currently open windows and load new ones
                        let mut live_hwnds = std::collections::HashSet::new();
                        for ws in &snap.workspaces {
                            for &h in &ws.client_hwnds {
                                live_hwnds.insert(h);
                                self.window_icon_textures.entry(h).or_insert_with(|| {
                                    tray::window_icon_rgba(h).map(|(w, h_px, px)| {
                                        ctx.load_texture(
                                            format!("ws-win-{h}"),
                                            egui::ColorImage::from_rgba_unmultiplied([w, h_px], &px),
                                            egui::TextureOptions::LINEAR,
                                        )
                                    })
                                });
                            }
                        }
                        self.window_icon_textures.retain(|k, _| live_hwnds.contains(k));

                        let highest_used = snap
                            .workspaces
                            .iter()
                            .filter(|w| w.occupied || w.id == snap.active_workspace)
                            .map(|w| w.id as usize)
                            .max()
                            .unwrap_or(1)
                            .max(5);

                        let mut widths = [0.0_f32; 10];
                        let mut visibility = [0.0_f32; 10];
                        for idx in 0..10 {
                            let ws_id = (idx + 1) as u32;
                            let info = snap.workspaces.get(idx);
                            let occupied = info.is_some_and(|w| w.occupied);
                            let active = snap.active_workspace == ws_id;
                            let shown = (idx + 1) <= highest_used || active || occupied;
                            let full = if occupied {
                                let n = info
                                    .map(|w| {
                                        if !w.client_hwnds.is_empty() {
                                            w.client_hwnds.len()
                                        } else {
                                            w.client_count
                                        }
                                    })
                                    .unwrap_or(1)
                                    .clamp(1, MAX_WS_ICONS);
                                pill_pad_x * 2.0
                                    + n as f32 * icon_size
                                    + (n.saturating_sub(1)) as f32 * icon_gap
                            } else if active {
                                empty_active_w
                            } else {
                                empty_dot_w
                            };
                            let target = if shown { full } else { 0.0 };
                            widths[idx] = ctx.animate_value_with_time(
                                egui::Id::new(("ws-width", idx)),
                                target,
                                anim_secs,
                            );
                            visibility[idx] = (widths[idx] / full.max(1.0)).clamp(0.0, 1.0);
                        }
                        let gaps: f32 = visibility.iter().map(|v| v * pill_gap).sum();
                        let total_w = widths.iter().sum::<f32>() + (gaps - pill_gap).max(0.0)
                            + inner_pad * 2.0;
                        let (ws_rect, ws_resp) = ui.allocate_exact_size(
                            egui::vec2(total_w, island_h),
                            egui::Sense::hover(),
                        );
                        paint_island(ui.painter(), ws_rect, &style, 0.0);

                        let mut pills = [egui::Rect::NOTHING; 10];
                        let mut x = ws_rect.left() + inner_pad;
                        for (idx, pill) in pills.iter_mut().enumerate() {
                            *pill = egui::Rect::from_min_size(
                                egui::pos2(x, ws_rect.center().y - pill_h / 2.0),
                                egui::vec2(widths[idx], pill_h),
                            );
                            x += widths[idx] + pill_gap * visibility[idx];
                        }
                        let pill_r = pill_h * 0.5;
                        let active_idx = (snap.active_workspace.clamp(1, 10) - 1) as usize;
                        let target = pills[active_idx];
                        let hl_x = ctx.animate_value_with_time(
                            egui::Id::new("ws-highlight-x"),
                            target.left(),
                            anim_secs,
                        );
                        let hl_w = ctx.animate_value_with_time(
                            egui::Id::new("ws-highlight-w"),
                            target.width(),
                            anim_secs,
                        );
                        let highlight = egui::Rect::from_min_size(
                            egui::pos2(hl_x, target.top()),
                            egui::vec2(hl_w, pill_h),
                        );

                        let mut hover_factors = [0.0_f32; 10];
                        for (idx, pill) in pills.iter().enumerate() {
                            if pill.width() < 1.0 {
                                continue;
                            }
                            let ws_id = (idx + 1) as u32;
                            let info = snap.workspaces.get(idx);
                            let occupied = info.is_some_and(|w| w.occupied);
                            let active = idx == active_idx;
                            let resp = ui.interact(
                                *pill,
                                egui::Id::new(("ws-pill", idx)),
                                egui::Sense::click(),
                            );
                            if resp.hovered() && !active && occupied {
                                hovered_ws = Some((ws_id, *pill));
                            }
                            let hover_t = ctx.animate_bool_with_time(
                                egui::Id::new(("ws-hover", idx)),
                                resp.hovered() && !active,
                                0.15,
                            );
                            hover_factors[idx] = hover_t;
                            if occupied {
                                ui.painter().rect_filled(*pill, pill_r, ws_occupied_bg);
                                if hover_t > 0.0 {
                                    ui.painter().rect_filled(
                                        *pill,
                                        pill_r,
                                        ws_active_bg.gamma_multiply(0.22 * hover_t),
                                    );
                                }
                            }

                            let client_count = info.map(|w| w.client_count).unwrap_or(0);
                            let hover_title = info
                                .and_then(|w| w.focused_title.as_deref())
                                .unwrap_or("empty");
                            let resp = resp.hover_card(format!(
                                "Workspace {} ({} window{}) — {}\nClick to switch • Scroll to cycle",
                                ws_id,
                                client_count,
                                if client_count == 1 { "" } else { "s" },
                                hover_title
                            ));
                            if resp.clicked() {
                                self.dispatch_wm_action(Action::Workspace(ws_id));
                            }
                        }

                        ui.painter().rect_filled(highlight, pill_r, ws_active_bg);

                        for (idx, pill) in pills.iter().enumerate() {
                            if pill.width() < 1.0 {
                                continue;
                            }
                            let info = snap.workspaces.get(idx);
                            let occupied = info.is_some_and(|w| w.occupied);
                            let active = idx == active_idx;
                            let covered = highlight.contains(pill.center());
                            let fg = if covered {
                                ws_active_fg
                            } else if occupied {
                                ws_occupied_fg
                            } else {
                                ws_empty_fg
                            }
                            .gamma_multiply(visibility[idx]);

                            if occupied {
                                let hwnds = info.map(|w| w.client_hwnds.as_slice()).unwrap_or(&[]);
                                let n = if !hwnds.is_empty() {
                                    hwnds.len().min(MAX_WS_ICONS)
                                } else {
                                    info.map(|w| w.client_count).unwrap_or(1).clamp(1, MAX_WS_ICONS)
                                };
                                let row_w = n as f32 * icon_size
                                    + (n.saturating_sub(1)) as f32 * icon_gap;
                                let start_x = pill.center().x - row_w * 0.5;
                                let clip_painter = ui.painter_at(*pill);
                                let uv = egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                );
                                let tint = egui::Color32::from_white_alpha(
                                    (255.0 * visibility[idx]).round() as u8,
                                );
                                for i in 0..n {
                                    let cx = start_x
                                        + icon_size * 0.5
                                        + i as f32 * (icon_size + icon_gap);
                                    let icon_rect = egui::Rect::from_center_size(
                                        egui::pos2(cx, pill.center().y),
                                        egui::vec2(icon_size, icon_size),
                                    );
                                    let tex = hwnds
                                        .get(i)
                                        .and_then(|h| self.window_icon_textures.get(h))
                                        .and_then(|t| t.as_ref());
                                    if let Some(t) = tex {
                                        clip_painter.image(t.id(), icon_rect, uv, tint);
                                    } else {
                                        chickenwm_core::ui_icons::paint_window(
                                            &clip_painter,
                                            icon_rect.shrink(1.0),
                                            fg,
                                        );
                                    }
                                }
                            } else if !active && !covered {
                                let hover_t = hover_factors[idx];
                                let dot_r = (3.0 + 1.2 * hover_t) * visibility[idx];
                                let dot_col = ws_empty_fg
                                    .lerp_to_gamma(fg_color, 0.55 * hover_t)
                                    .gamma_multiply(visibility[idx]);
                                ui.painter().circle_filled(pill.center(), dot_r, dot_col);
                            }
                        }

                        if ws_resp.hovered() {
                            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
                            if scroll_y > 4.0 {
                                self.dispatch_wm_action(Action::WorkspaceRelative(-1));
                            } else if scroll_y < -4.0 {
                                self.dispatch_wm_action(Action::WorkspaceRelative(1));
                            }
                        }

                        let is_master = snap.active_layout.eq_ignore_ascii_case("master");
                        let layout_label = if is_master { "master" } else { "dwindle" };
                        if island_vector(
                            ui,
                            egui::Id::new("island-layout"),
                            &style,
                            Some(layout_label),
                            font.clone(),
                            accent_secondary,
                            island_h,
                            true,
                            Some(if is_master {
                                chickenwm_core::ui_icons::paint_layout_master
                            } else {
                                chickenwm_core::ui_icons::paint_layout_dwindle
                            }),
                        )
                        .hover_card("Click to toggle layout (Dwindle <-> Master)")
                        .clicked()
                        {
                            let next = if is_master { "dwindle" } else { "master" };
                            self.dispatch_wm_action(Action::SetLayout(next.to_string()));
                        }
                    });
                    let left_end = left_ui.min_rect().right();

                    // =========================================================================
                    // RIGHT: widgets from the library (`--modules-right`)
                    // =========================================================================
                    let mut right_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(full)
                            .layout(egui::Layout::right_to_left(egui::Align::Center)),
                    );
                    right_ui.spacing_mut().item_spacing.x = island_gap;
                    let env = WidgetEnv {
                        style: &style,
                        font: font.clone(),
                        fg: fg_color,
                        subtext: subtext_color,
                        island_h,
                        snap: &snap,
                        css: &css,
                    };
                    // Listed left to right; the layout fills from the right edge.
                    for (i, slot) in widgets::right_layout(&css).into_iter().enumerate().rev() {
                        match slot {
                            widgets::Slot::Single(widget) => self.draw_widget(&mut right_ui, widget, i, false, &env),
                            widgets::Slot::Group(members) => {
                                // One island behind all members, painted once their extent is known.
                                let bg = right_ui.painter().add(egui::Shape::Noop);
                                let rect = right_ui
                                    .scope(|ui| {
                                        ui.spacing_mut().item_spacing.x = 0.0;
                                        ui.add_space(3.0);
                                        for widget in members.into_iter().rev() {
                                            self.draw_widget(ui, widget, i, true, &env);
                                        }
                                        ui.add_space(3.0);
                                    })
                                    .response
                                    .rect;
                                let island = egui::epaint::RectShape::new(
                                    rect,
                                    style.radius,
                                    style.bg,
                                    egui::Stroke::new(1.0_f32, style.border),
                                    egui::StrokeKind::Inside,
                                );
                                if !style.flat {
                                    right_ui.painter().set(bg, island);
                                }
                            }
                        }
                    }
                    let right_start = right_ui.min_rect().left();

                    // =========================================================================
                    // CENTER: focused window title, in the space the two sides leave free.
                    // Centred on the bar when it fits, otherwise in the middle of the gap.
                    // =========================================================================
                    let (lo, hi) = (left_end + island_gap * 2.0, right_start - island_gap * 2.0);
                    let half = (full.center().x - lo).min(hi - full.center().x);
                    let gap = if half * 2.0 >= 240.0 {
                        egui::Rect::from_x_y_ranges(full.center().x - half..=full.center().x + half, full.y_range())
                    } else {
                        egui::Rect::from_x_y_ranges(lo..=hi.max(lo), full.y_range())
                    };
                    let mut center_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(gap)
                            .layout(egui::Layout::top_down(egui::Align::Center)),
                    );
                    if gap.width() >= 40.0 {
                        let ui = &mut center_ui;
                        let text = title_text;
                        let galley = ui.painter().layout_no_wrap(text, font.clone(), fg_color);
                        let icon_w = 14.0_f32;
                        let gap_px = 6.0_f32;
                        let bold_extra = if bold { 1.0 } else { 0.0 };
                        let content_w = icon_w + gap_px + galley.size().x + bold_extra;
                        let target_w = (content_w + 24.0).min(ui.available_width());
                        let width = ctx.animate_value_with_time(
                            egui::Id::new("island-title-w"),
                            target_w,
                            anim_secs,
                        );
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(width, island_h),
                            egui::Sense::hover(),
                        );
                        paint_island(ui.painter(), rect, &style, 0.0);
                        let start_x = rect.center().x - content_w / 2.0;
                        let icon_rect = egui::Rect::from_center_size(
                            egui::pos2(start_x + icon_w / 2.0, rect.center().y),
                            egui::vec2(icon_w, icon_w),
                        );
                        chickenwm_core::ui_icons::paint_window(ui.painter(), icon_rect, fg_color.gamma_multiply(title_fade));
                        let pos = egui::pos2(
                            start_x + icon_w + gap_px,
                            rect.center().y - galley.size().y / 2.0 + (1.0 - title_fade) * 4.0,
                        );
                        draw_text(
                            &ui.painter_at(rect.shrink(4.0)),
                            pos,
                            galley,
                            fg_color.gamma_multiply(title_fade),
                            bold,
                        );
                    }
                }
                if !is_float && bar_bg.a() > 0 {
                    let panel_rect = ctx.screen_rect();
                    let y = panel_rect.bottom() - 0.5;
                    ctx.layer_painter(egui::LayerId::new(
                        egui::Order::Foreground,
                        egui::Id::new("roobar-bottom-border"),
                    ))
                    .hline(panel_rect.x_range(), y, egui::Stroke::new(1.0_f32, border_color));
                }
            });

        // Test hook (headless renders): ROOBAR_TEST_POPOVER=calendar|volume|processes|media|network|menu|about opens
        // that popover on the first frame.
        static TEST_POPOVER: std::sync::Once = std::sync::Once::new();
        TEST_POPOVER.call_once(|| {
            let kind = match std::env::var("ROOBAR_TEST_POPOVER").unwrap_or_default().as_str() {
                "calendar" => Some(PopoverKind::Calendar),
                "volume" => Some(PopoverKind::Volume),
                "processes" => Some(PopoverKind::Processes),
                "media" => Some(PopoverKind::Media),
                "network" => Some(PopoverKind::Network),
                "menu" => Some(PopoverKind::Menu),
                "about" => Some(PopoverKind::About),
                _ => None,
            };
            if let Some(kind) = kind {
                let w = ctx.screen_rect().width();
                self.popover = Some(Popover::new(ctx, kind, egui::Rect::from_min_size(egui::pos2(w - 300.0, 0.0), egui::vec2(120.0, 30.0))));
            }
        });

        // Workspace previews (not while a popover is open).
        self.previews.hovered(if self.popover.is_none() { hovered_ws } else { None });
        if let Some(pv) = self.previews.current.as_mut() {
            let bar = ctx.input(|i| i.viewport().outer_rect).unwrap_or(ctx.screen_rect());
            pv.show(ctx, bar, self.screen_rect, island_bg.to_opaque(), border_color, fg_color, pill_radius, egui::FontId::proportional((font_size - 1.0).max(9.0)));
        }
        if self.previews.wants_repaint() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        if let Some(p) = self.popover.as_mut() {
            let bar = ctx.input(|i| i.viewport().outer_rect).unwrap_or(ctx.screen_rect());
            let pstyle = popover::PopoverStyle {
                bg: island_bg.to_opaque(),
                border: border_color,
                fg: fg_color,
                subtext: subtext_color,
                accent_bg: ws_active_bg,
                accent_fg: ws_active_fg,
                radius: pill_radius,
                font: font.clone(),
                nerd: self.nerd_icons,
                anim_secs,
            };
            let out = p.show(ctx, bar, self.screen_rect, &pstyle, &self.status.snapshot());
            for cmd in out.cmds {
                self.status.send(cmd);
            }
            if let Some(uri) = out.open_uri {
                status::open_uri(uri);
            }
            let anchor = p.anchor;
            if out.close {
                self.popover_closed = Some((p.kind, Instant::now()));
                self.popover = None;
            }
            // Acted on once the menu has faded out, so its window isn't caught in the power
            // overlay's backdrop and About opens in its place.
            if let Some(action) = out.menu {
                self.pending_menu = Some(action);
            }
            if self.popover.is_none() {
                if let Some(action) = self.pending_menu.take() {
                    match action {
                        popover::MenuAction::Files => spawn_sibling("chickenwm-files.exe"),
                        popover::MenuAction::Settings => spawn_sibling("chickenwm-settings.exe"),
                        popover::MenuAction::AboutChickenWm => {
                            self.popover = Some(Popover::new(ctx, PopoverKind::About, anchor));
                        }
                        popover::MenuAction::Shutdown => {
                            if self.power_overlay.is_none() {
                                let backdrop = Backdrop::capture(ctx, self.screen_rect);
                                self.power_overlay =
                                    Some(PowerOverlay { opened: Instant::now(), closing: None, shown: false, backdrop });
                            }
                        }
                    }
                }
            }
        }

        // Hover cards (bar widgets, popover rows) in their own window so the bar doesn't clip
        // them. Bar cards stay hidden while a popover is open or the power overlay is up.
        self.hover_cards.show(
            ctx,
            self.screen_rect,
            island_bg.to_opaque(),
            border_color,
            fg_color,
            pill_radius,
            egui::FontId::proportional((font_size - 1.0).max(9.0)),
            self.power_overlay.is_some(),
            self.popover.is_some().then_some(bar_outer),
        );

        // Power overlay: a transparent full-monitor window that dims the screen and shows the
        // options as cards. Closes on Esc, a click on the dimmed area, or a pick (fade out first).
        if let Some(ov) = self.power_overlay.as_mut() {
            const CARD: f32 = 64.0;
            const CARD_GAP: f32 = 24.0;
            const STAGGER_SECS: f32 = 0.06;
            let ease = |t: f32| 1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3);
            let open_secs = ov.opened.elapsed().as_secs_f32();
            let close_t = ov
                .closing
                .map(|(at, _)| at.elapsed().as_secs_f32() / (anim_secs * 0.8))
                .unwrap_or(0.0);
            // Nothing is painted until the window's DWM transition is off, so Windows' own
            // open animation (if it still plays) has only transparent pixels to animate.
            let fade_out = (1.0 - ease(close_t)) * if ov.shown { 1.0 } else { 0.0 };
            let dim = ease(open_secs / anim_secs) * fade_out;
            let last_card_done = open_secs >= anim_secs + STAGGER_SECS * 2.0;
            let animating = !last_card_done || ov.closing.is_some();

            let builder = egui::ViewportBuilder::default()
                .with_title(POWER_OVERLAY_TITLE)
                // Created visible: a transparent window that starts hidden can come up opaque black.
                .with_visible(true)
                .with_decorations(false)
                .with_transparent(true)
                .with_always_on_top()
                .with_resizable(false)
                .with_taskbar(false)
                .with_active(true)
                .with_position(self.screen_rect.min)
                // 1 px short of the monitor so it isn't treated as fullscreen (see PowerOverlay).
                .with_inner_size(self.screen_rect.size() - egui::vec2(0.0, 1.0));

            let (picked, dismissed) = ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("power-overlay"),
                builder,
                |octx, _class| {
                    let mut picked = None;
                    let mut dismissed = false;
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE)
                        .show(octx, |ui| {
                            let full = ui.max_rect();
                            let backdrop = ui.interact(
                                full,
                                egui::Id::new("power-backdrop"),
                                egui::Sense::click(),
                            );

                            if let Some(bd) = &ov.backdrop {
                                // The screen as it was (seamless at open/close), cross-fading
                                // into a dimmed, frosted copy. The window is 1px short of the
                                // monitor, so crop rather than squash.
                                let uv = egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, (full.height() / self.screen_rect.height()).min(1.0)),
                                );
                                ui.painter().image(bd.sharp.id(), full, uv, egui::Color32::WHITE);
                                let v = (150.0 * dim) as u8;
                                let a = (255.0 * dim) as u8;
                                ui.painter().image(
                                    bd.blurred.id(),
                                    full,
                                    uv,
                                    egui::Color32::from_rgba_premultiplied(v, v, v, a),
                                );
                            } else {
                                ui.painter().rect_filled(
                                    full,
                                    0.0,
                                    egui::Color32::from_black_alpha((160.0 * dim) as u8),
                                );
                            }

                            let row_w = CARD * 3.0 + CARD_GAP * 2.0;
                            let origin = full.center() - egui::vec2(row_w / 2.0, CARD / 2.0);
                            for (i, action) in PowerAction::ALL.into_iter().enumerate() {
                                // Cards rise into place one after another.
                                let appear = ease(
                                    (open_secs - i as f32 * STAGGER_SECS) / anim_secs,
                                ) * fade_out;
                                let slot = egui::Rect::from_min_size(
                                    origin
                                        + egui::vec2(
                                            i as f32 * (CARD + CARD_GAP),
                                            (1.0 - appear) * 24.0,
                                        ),
                                    egui::vec2(CARD, CARD),
                                );
                                let resp = ui.interact(
                                    slot,
                                    egui::Id::new(("power-card", i)),
                                    egui::Sense::click(),
                                );
                                let hover_t = octx.animate_bool_with_time(
                                    egui::Id::new(("power-card-hover", i)),
                                    resp.hovered(),
                                    0.15,
                                );
                                // Hover lifts the card and grows it slightly.
                                let rect = egui::Rect::from_center_size(
                                    slot.center() - egui::vec2(0.0, 4.0 * hover_t),
                                    egui::vec2(CARD, CARD) * (1.0 + 0.08 * hover_t),
                                );
                                let card_style = IslandStyle {
                                    bg: style.bg.gamma_multiply(appear),
                                    hover_bg: style.hover_bg.gamma_multiply(appear),
                                    border: style.border.gamma_multiply(appear),
                                    radius: style.radius,
                                    bold: style.bold,
                                    flat: false,
                                };
                                let painter = ui.painter();
                                paint_island(painter, rect, &card_style, hover_t);
                                let glyph = fg_color.gamma_multiply(appear);
                                let r = rect.width() * 0.22;
                                match action {
                                    PowerAction::Shutdown => {
                                        paint_power_glyph(painter, rect.center(), r, glyph)
                                    }
                                    PowerAction::Restart => {
                                        paint_restart_glyph(painter, rect.center(), r, glyph)
                                    }
                                    PowerAction::LogOut => {
                                        paint_logout_glyph(painter, rect.center(), r, glyph)
                                    }
                                }
                                painter.text(
                                    egui::pos2(slot.center().x, slot.bottom() + 12.0),
                                    egui::Align2::CENTER_TOP,
                                    action.label(),
                                    font.clone(),
                                    style.bg.gamma_multiply(appear),
                                );
                                if resp.clicked() {
                                    picked = Some(action);
                                }
                            }
                            if backdrop.clicked() && picked.is_none() {
                                dismissed = true;
                            }
                        });
                    if octx.input(|i| {
                        i.key_pressed(egui::Key::Escape) || i.viewport().close_requested()
                    }) {
                        dismissed = true;
                    }
                    if animating {
                        octx.request_repaint();
                    }
                    (picked, dismissed)
                },
            );

            if !ov.shown {
                // The window now exists (still blank): turn off Windows' transition, then start
                // our own fade from this moment.
                if prepare_power_overlay_window() {
                    ov.shown = true;
                    ov.opened = Instant::now();
                }
                ctx.request_repaint();
            } else if ov.closing.is_none() {
                if picked.is_some() || dismissed {
                    ov.closing = Some((Instant::now(), picked));
                }
            } else if close_t >= 1.0 {
                let action = ov.closing.and_then(|(_, a)| a);
                self.power_overlay = None;
                if let Some(action) = action {
                    run_power_action(action);
                }
            }
        }

        // egui keeps repainting while any animate_* value is moving; this covers the clock/stats.
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing("roobar");
    let _failsafe = spawn_emergency_failsafe_thread();
    let _ = bootstrap_config_and_themes();

    let css = load_css_sheet("roobar.css").unwrap_or_default();
    let is_float = widgets::css_flag(&css, "--float", true);
    let bar_height = css.var_px("--bar-height", 36.0);
    let margin_top = if is_float { css.var_px("--bar-margin-top", 6.0) } else { 0.0 };
    let margin_x = if is_float { css.var_px("--bar-margin-x", 10.0) } else { 0.0 };

    // Fetch initial workspace state from `chickenwm-wm` if running
    let initial_snapshot = match send_ipc_command_blocking(PIPE_NAME_WM, &IpcCommand::QueryBarState)
    {
        Ok(IpcResponse::BarState(snap)) => snap,
        _ => BarStateSnapshot::default(),
    };

    let shared = Arc::new(Mutex::new(SharedBarState {
        snapshot: initial_snapshot,
        css,
        egui_ctx: None,
    }));

    // `--render-to out.png`: draw the bar headless (no window, and none of the bar's IPC, so a
    // running bar is left alone) and exit.
    let primary = enumerate_monitors().into_iter().find(|m| m.is_primary);
    let primary_w = primary.as_ref().map_or(1920.0, |m| m.width as f32);
    if let Some(req) = chickenwm_core::headless::RenderRequest::from_args([(primary_w - margin_x * 2.0).max(400.0), bar_height]) {
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(primary_w, 1080.0));
        return chickenwm_core::headless::render_app(&req, |_cc| RooBarApp::new(shared, screen_rect));
    }

    // Watch `~/.config/chickenwm/roobar.css` for <5ms live hot-reload
    let shared_css = Arc::clone(&shared);
    let _watcher = spawn_config_watcher(move |ev| {
        if let ConfigWatchEvent::CssChanged { file_name, .. } = ev {
            if file_name.eq_ignore_ascii_case("roobar.css") {
                if let Ok(new_sheet) = load_css_sheet("roobar.css") {
                    let lock = shared_css.lock();
                    let ctx_opt = lock.egui_ctx.clone();
                    drop(lock);
                    shared_css.lock().css = new_sheet;
                    if let Some(c) = ctx_opt {
                        c.request_repaint();
                    }
                }
            }
        }
    });

    // Spawn direct IPC server on `\\.\pipe\chickenwm-bar-ipc` (`PIPE_NAME_BAR`)
    // so `chickenwm-wm` pushes workspace switches in <1ms!
    let shared_ipc = Arc::clone(&shared);
    tokio::spawn(async move {
        let _ = serve_ipc(PIPE_NAME_BAR, move |cmd| {
            let st = Arc::clone(&shared_ipc);
            async move {
                match cmd {
                    IpcCommand::NotifyBarState(snap) => {
                        let ctx_opt = {
                            let mut lock = st.lock();
                            lock.snapshot = snap;
                            lock.egui_ctx.clone()
                        };
                        if let Some(c) = ctx_opt {
                            c.request_repaint();
                        }
                        IpcResponse::Ok {
                            message: "roobar state updated".to_string(),
                        }
                    }
                    IpcCommand::ReloadConfig => {
                        if let Ok(new_sheet) = load_css_sheet("roobar.css") {
                            let ctx_opt = {
                                let mut lock = st.lock();
                                lock.css = new_sheet;
                                lock.egui_ctx.clone()
                            };
                            if let Some(c) = ctx_opt {
                                c.request_repaint();
                            }
                        }
                        IpcResponse::Ok {
                            message: "roobar.css reloaded".to_string(),
                        }
                    }
                    _ => IpcResponse::Ok {
                        message: "Ignored by roobar".to_string(),
                    },
                }
            }
        })
        .await;
    });

    let monitors = enumerate_monitors();
    let mon = monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .cloned()
        .unwrap_or(chickenwm_core::ipc::MonitorInfo {
            id: 0,
            name: r"\\.\DISPLAY1".to_string(),
            adapter_name: "Default".to_string(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            work_rect: [0, 0, 1920, 1080],
            refresh_rate_hz: 60,
            dpi_scale: 1.0,
            active_workspace_id: 1,
            is_primary: true,
        });
    let bar_x = if is_float { (mon.x as f32) + margin_x } else { mon.x as f32 };
    let bar_y = if is_float { (mon.y as f32) + margin_top } else { mon.y as f32 };
    let bar_w = if is_float { ((mon.width as f32) - margin_x * 2.0).max(400.0) } else { mon.width as f32 };
    let screen_rect = egui::Rect::from_min_size(
        egui::pos2(mon.x as f32, mon.y as f32),
        egui::vec2(mon.width as f32, mon.height as f32),
    );

    info!(
        "Starting roobar at ({}, {}), size {}x{} on {} ({}Hz)",
        bar_x, bar_y, bar_w, bar_height, mon.name, mon.refresh_rate_hz
    );

    let viewport = egui::ViewportBuilder::default()
            .with_icon(egui::IconData { rgba: include_bytes!("../assets/icon-64.rgba").to_vec(), width: 64, height: 64 })
        .with_title("ChickenWM Bar — roobar")
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_resizable(false)
        .with_taskbar(false)
        .with_position([bar_x, bar_y])
        .with_inner_size([bar_w, bar_height]);

    let native_options = eframe::NativeOptions {
        // glow, not wgpu: egui_kittest (headless feature) also enables eframe's wgpu backend,
        // which eframe would then prefer, and wgpu draws transparent windows black on Windows.
        renderer: eframe::Renderer::Glow,
        viewport,
        ..Default::default()
    };

    let _ = eframe::run_native(
        "ChickenWM Bar — roobar",
        native_options,
        Box::new(move |cc| {
            if let Ok(handle) = cc.window_handle() {
                if let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw() {
                    let hwnd = windows::Win32::Foundation::HWND(win32.hwnd.get() as *mut _);
                    prepare_hwnd_no_transitions(hwnd);
                }
            }
            Ok(Box::new(RooBarApp::new(shared, screen_rect)))
        }),
    );

    Ok(())
}
