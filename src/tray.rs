//! System tray icons on the bar: turning an `HICON` into pixels, and sending clicks back to the
//! app that owns the icon the way Explorer's taskbar does.

use chickenwm_core::ipc::TrayIcon;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, GetIconInfo, PostMessageW, HICON, ICONINFO};

/// 32-bit pixels of a bitmap, top-down, BGRA.
unsafe fn bitmap_pixels(bmp: HBITMAP) -> Option<(usize, usize, Vec<u8>)> {
    let mut info = BITMAP::default();
    if unsafe { GetObjectW(bmp, std::mem::size_of::<BITMAP>() as i32, Some(&mut info as *mut _ as *mut _)) } == 0 {
        return None;
    }
    let (w, h) = (info.bmWidth as usize, info.bmHeight.unsigned_abs() as usize);
    if w == 0 || h == 0 || w > 256 || h > 512 {
        return None;
    }
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; w * h * 4];
    let dc = unsafe { GetDC(None) };
    let lines = unsafe { GetDIBits(dc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS) };
    unsafe { ReleaseDC(None, dc) };
    (lines > 0).then_some((w, h, buf))
}

/// RGBA pixels of an icon: (width, height, rgba).
pub fn icon_rgba(icon: u64) -> Option<(usize, usize, Vec<u8>)> {
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(HICON(icon as usize as *mut core::ffi::c_void), &mut ii).ok()?;
        let color = (!ii.hbmColor.is_invalid()).then(|| bitmap_pixels(ii.hbmColor)).flatten();
        let mask = bitmap_pixels(ii.hbmMask);
        let _ = DeleteObject(ii.hbmColor);
        let _ = DeleteObject(ii.hbmMask);
        let mask_px = mask.clone();
        let (w, h, mut px) = match (color, mask) {
            (Some(c), _) => c,
            // Monochrome icon: the mask is twice as tall (AND mask, then XOR image).
            (None, Some((w, h2, m))) => {
                let h = h2 / 2;
                let mut px = vec![0u8; w * h * 4];
                for i in 0..w * h {
                    let and = m[i * 4] > 127;
                    let xor = m[(i + w * h) * 4] > 127;
                    let v = if xor { 255 } else { 0 };
                    px[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, if and && !xor { 0 } else { 255 }]);
                }
                (w, h, px)
            }
            _ => return None,
        };
        // BGRA → RGBA; icons without an alpha channel take it from the AND mask.
        let has_alpha = px.chunks_exact(4).any(|p| p[3] != 0);
        for (i, p) in px.chunks_exact_mut(4).enumerate() {
            p.swap(0, 2);
            if !has_alpha {
                let transparent = mask_px.as_ref().is_some_and(|(_, _, m)| m.get(i * 4).is_some_and(|&v| v > 127));
                p[3] = if transparent { 0 } else { 255 };
            }
        }
        Some((w, h, px))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Click {
    Left,
    Right,
    Double,
}

/// Sends a click to the icon's owner the way the shell does (`NOTIFYICON_VERSION_4` packs the
/// message and icon id into lParam and the cursor position into wParam).
pub fn send_click(icon: &TrayIcon, click: Click, screen: (i32, i32)) {
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_LBUTTONDBLCLK: u32 = 0x0203;
    const WM_RBUTTONDOWN: u32 = 0x0204;
    const WM_RBUTTONUP: u32 = 0x0205;
    const WM_CONTEXTMENU: u32 = 0x007B;
    const NIN_SELECT: u32 = 0x0400;
    if icon.callback_message == 0 {
        return;
    }
    // Let the app bring its menu / window to the front.
    unsafe {
        let _ = AllowSetForegroundWindow(icon.pid);
    }
    let hwnd = HWND(icon.hwnd as usize as *mut core::ffi::c_void);
    let messages: &[u32] = match click {
        Click::Left => &[WM_LBUTTONDOWN, WM_LBUTTONUP],
        Click::Double => &[WM_LBUTTONDBLCLK],
        Click::Right => &[WM_RBUTTONDOWN, WM_RBUTTONUP],
    };
    let post = |msg: u32| unsafe {
        let (w, l) = if icon.version >= 4 {
            let xy = ((screen.1 as u32 & 0xFFFF) << 16) | (screen.0 as u32 & 0xFFFF);
            (WPARAM(xy as usize), LPARAM((((icon.uid & 0xFFFF) << 16) | (msg & 0xFFFF)) as isize))
        } else {
            (WPARAM(icon.uid as usize), LPARAM(msg as isize))
        };
        let _ = PostMessageW(hwnd, icon.callback_message, w, l);
    };
    for &m in messages {
        post(m);
    }
    if icon.version >= 4 {
        match click {
            Click::Left => post(NIN_SELECT),
            Click::Right => post(WM_CONTEXTMENU),
            Click::Double => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn system_icon_converts() {
        use windows::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION, IDI_WARNING};
        for id in [IDI_APPLICATION, IDI_WARNING] {
            let icon = unsafe { LoadIconW(None, id) }.unwrap();
            let (w, h, px) = super::icon_rgba(icon.0 as usize as u64).expect("icon pixels");
            assert!(w >= 16 && h >= 16 && px.len() == w * h * 4);
            // Some pixels are see-through, some solid.
            assert!(px.chunks_exact(4).any(|p| p[3] == 0) || id == IDI_APPLICATION);
            assert!(px.chunks_exact(4).any(|p| p[3] > 200));
        }
    }
}
