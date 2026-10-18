//! Workspace previews: hovering a workspace pill shows live thumbnails of the windows on that
//! workspace (DWM thumbnails, so they keep updating; ChickenWM parks hidden workspaces off
//! screen rather than hiding them, so DWM still renders them).

use chickenwm_core::ipc::{send_ipc_command_blocking, ClientInfo, IpcCommand, IpcResponse, PIPE_NAME_WM};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES,
    DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
};

const TITLE: &str = "roobar workspace preview";
const THUMB_W: f32 = 220.0;
const MAX_THUMBS: usize = 6;

pub struct Preview {
    pub ws: u32,
    anchor: egui::Rect,
    clients: Vec<ClientInfo>,
    /// (window, DWM thumbnail id)
    thumbs: Vec<(u64, isize)>,
    dest: Option<isize>,
}

impl Drop for Preview {
    fn drop(&mut self) {
        for (_, t) in self.thumbs.drain(..) {
            unsafe {
                let _ = DwmUnregisterThumbnail(t);
            }
        }
    }
}

/// Tracks hovering over pills and opens / closes the preview after a short delay.
#[derive(Default)]
pub struct Previews {
    pub current: Option<Preview>,
    hover: Option<(u32, egui::Rect, Instant)>,
    left_at: Option<Instant>,
}

impl Previews {
    /// Call once per frame with the hovered (non-active) workspace pill, if any.
    pub fn hovered(&mut self, ws: Option<(u32, egui::Rect)>) {
        match ws {
            Some((id, rect)) => {
                self.left_at = None;
                if self.hover.as_ref().is_none_or(|h| h.0 != id) {
                    self.hover = Some((id, rect, Instant::now()));
                }
                let (hid, hrect, since) = self.hover.unwrap();
                if since.elapsed() >= Duration::from_millis(350) && self.current.as_ref().is_none_or(|p| p.ws != hid) {
                    let clients: Vec<ClientInfo> = match send_ipc_command_blocking(PIPE_NAME_WM, &IpcCommand::QueryClients) {
                        Ok(IpcResponse::Clients(c)) => c.into_iter().filter(|c| c.workspace_id == hid).take(MAX_THUMBS).collect(),
                        _ => Vec::new(),
                    };
                    self.current = (!clients.is_empty()).then(|| Preview { ws: hid, anchor: hrect, clients, thumbs: Vec::new(), dest: None });
                }
            }
            None => {
                self.hover = None;
                let left = *self.left_at.get_or_insert_with(Instant::now);
                if left.elapsed() > Duration::from_millis(150) {
                    self.current = None;
                }
            }
        }
    }

    pub fn wants_repaint(&self) -> bool {
        self.hover.is_some() || self.current.is_some()
    }
}

/// Thumbnail size for a window, keeping its aspect ratio.
fn thumb_size(c: &ClientInfo) -> egui::Vec2 {
    let (w, h) = (c.rect[2].max(1) as f32, c.rect[3].max(1) as f32);
    egui::vec2(THUMB_W, (THUMB_W * h / w).clamp(80.0, 200.0))
}

impl Preview {
    pub fn show(&mut self, ctx: &egui::Context, bar: egui::Rect, screen: egui::Rect, bg: egui::Color32, border: egui::Color32, fg: egui::Color32, radius: f32, font: egui::FontId) {
        let pad = 12.0;
        let gap = 10.0;
        let label_h = 22.0;
        let thumbs: Vec<egui::Vec2> = self.clients.iter().map(thumb_size).collect();
        let w = thumbs.iter().map(|s| s.x).sum::<f32>() + gap * (thumbs.len().max(1) - 1) as f32 + pad * 2.0;
        let h = thumbs.iter().map(|s| s.y).fold(0.0, f32::max) + label_h + pad * 2.0;
        let x = (bar.left() + self.anchor.center().x - w / 2.0).clamp(screen.left() + 6.0, (screen.right() - w - 6.0).max(screen.left()));
        let builder = egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_resizable(false)
            .with_taskbar(false)
            .with_active(false)
            .with_mouse_passthrough(true)
            .with_position(egui::pos2(x, bar.bottom() + 6.0))
            .with_inner_size(egui::vec2(w, h));
        let mut slots: Vec<egui::Rect> = Vec::new();
        let mut ppp = 1.0;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("roobar-ws-preview"), builder, |pctx, _| {
            ppp = pctx.pixels_per_point();
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(bg).stroke(egui::Stroke::new(1.0_f32, border)).corner_radius(radius))
                .show(pctx, |ui| {
                    let mut cx = ui.max_rect().left() + pad;
                    let top = ui.max_rect().top() + pad;
                    for (c, size) in self.clients.iter().zip(&thumbs) {
                        let slot = egui::Rect::from_min_size(egui::pos2(cx, top), *size);
                        ui.painter().rect_filled(slot, 6.0, fg.gamma_multiply(0.06));
                        let title: String = c.title.chars().take(30).collect();
                        ui.painter().with_clip_rect(egui::Rect::from_min_size(egui::pos2(cx, slot.bottom()), egui::vec2(size.x, label_h)))
                            .text(egui::pos2(slot.center().x, slot.bottom() + label_h / 2.0 + 2.0), egui::Align2::CENTER_CENTER, title, font.clone(), fg);
                        slots.push(slot);
                        cx += size.x + gap;
                    }
                });
        });
        self.attach_thumbnails(&slots, ppp);
    }

    /// Registers the DWM thumbnails once the preview window exists, and places them in `slots`.
    fn attach_thumbnails(&mut self, slots: &[egui::Rect], ppp: f32) {
        if self.dest.is_none() {
            use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
            let Ok(hwnd) = (unsafe { FindWindowW(None, &windows::core::HSTRING::from(TITLE)) }) else { return };
            if hwnd.0.is_null() {
                return;
            }
            self.dest = Some(hwnd.0 as isize);
            for c in &self.clients {
                let src = HWND(c.hwnd as usize as *mut core::ffi::c_void);
                if let Ok(id) = unsafe { DwmRegisterThumbnail(hwnd, src) } {
                    self.thumbs.push((c.hwnd, id));
                }
            }
        }
        for ((hwnd, id), slot) in self.thumbs.iter().zip(slots) {
            let _ = hwnd;
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: RECT {
                    left: (slot.left() * ppp).round() as i32,
                    top: (slot.top() * ppp).round() as i32,
                    right: (slot.right() * ppp).round() as i32,
                    bottom: (slot.bottom() * ppp).round() as i32,
                },
                opacity: 255,
                fVisible: true.into(),
                fSourceClientAreaOnly: false.into(),
                ..Default::default()
            };
            unsafe {
                let _ = DwmUpdateThumbnailProperties(*id, &props);
            }
        }
    }
}
