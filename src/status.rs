//! System status for the bar's network, volume, Bluetooth and media widgets.
//!
//! One background thread owns the COM / WinRT objects and polls: the volume every few hundred ms
//! (so media keys show up promptly), media every second, the network, Bluetooth and output
//! devices every few seconds. Changes asked for by the bar (mute, volume, output device,
//! play/pause) go through the same thread.

use parking_lot::Mutex;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Win32::Devices::Bluetooth::{
    BluetoothFindDeviceClose, BluetoothFindFirstDevice, BluetoothFindFirstRadio, BluetoothFindNextDevice,
    BluetoothFindRadioClose, BLUETOOTH_DEVICE_INFO, BLUETOOTH_DEVICE_SEARCH_PARAMS, BLUETOOTH_FIND_RADIO_PARAMS,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::WiFi::{
    wlan_interface_state_connected, wlan_intf_opcode_current_connection, WlanCloseHandle, WlanEnumInterfaces,
    WlanFreeMemory, WlanGetAvailableNetworkList, WlanOpenHandle, WlanQueryInterface, WLAN_AVAILABLE_NETWORK_LIST,
    WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST,
};
use windows::Win32::Networking::WinSock::AF_UNSPEC;
use windows::core::PCWSTR;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const IF_TYPE_ETHERNET: u32 = 6;
const IF_TYPE_WIFI: u32 = 71;
const IF_OPER_STATUS_UP: i32 = 1;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Network {
    #[default]
    Disconnected,
    Ethernet {
        adapter: String,
        speed_mbps: u32,
        ip: String,
    },
    Wifi {
        ssid: String,
        signal: u32,
        speed_mbps: u32,
        ip: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub signal: u32,
    pub connected: bool,
    pub secured: bool,
    pub profile: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Volume {
    /// 0.0 ..= 1.0
    pub level: f32,
    pub muted: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Bluetooth {
    /// No Bluetooth radio: the widget hides itself.
    #[default]
    Unavailable,
    Off,
    On { devices: Vec<String> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battery {
    /// 0 ..= 100
    pub percent: u8,
    pub charging: bool,
    pub ac_online: bool,
    /// Estimated lifetime remaining in seconds, if known
    pub remaining_seconds: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub network: Network,
    pub wifi_networks: Vec<WifiNetwork>,
    /// `None` when there is no output device.
    pub volume: Option<Volume>,
    pub bluetooth: Bluetooth,
    /// `None` on desktop PCs, VMs, or devices without a battery.
    pub battery: Option<Battery>,
    /// Active playback devices; the default one is marked.
    pub outputs: Vec<Output>,
    /// The current media session (what Windows' media flyout shows), if any.
    pub media: Option<Media>,
    /// All active media sessions for per-player selection.
    pub media_players: Vec<Media>,
    pub selected_player: Option<String>,
    /// Notification-area icons hosted by rooster.
    pub tray: Vec<chickenwm_core::ipc::TrayIcon>,
    /// Unread notifications and do-not-disturb (chickenwm-notify).
    pub unread: u32,
    pub dnd: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub id: String,
    pub name: String,
    pub default: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
    /// Player app, e.g. `Spotify.exe` or a browser's id.
    pub app: String,
    pub position_secs: f32,
    pub duration_secs: f32,
}

pub enum Cmd {
    ToggleMute,
    /// Relative change, -1.0 ..= 1.0.
    Adjust(f32),
    /// Absolute level, 0.0 ..= 1.0.
    SetVolume(f32),
    /// Makes this device (an `Output::id`) the default output.
    SetOutput(String),
    PlayPause,
    Next,
    Previous,
    /// Seek active media player to position in seconds.
    SeekMedia(f32),
    /// Switch active media player by app id.
    SelectPlayer(String),
    /// Connect to a saved Wi-Fi profile or SSID.
    ConnectWifi(String),
    /// Refresh Wi-Fi network list immediately.
    RefreshWifi,
}

pub struct StatusMonitor {
    pub status: Arc<Mutex<Status>>,
    cmds: Sender<Cmd>,
}

impl StatusMonitor {
    /// Starts the polling thread. `on_change` runs (on that thread) whenever the status changes.
    pub fn spawn(on_change: impl Fn() + Send + 'static) -> Self {
        let status = Arc::new(Mutex::new(Status::default()));
        let (cmds, rx) = channel::<Cmd>();
        let shared = Arc::clone(&status);
        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let mut wlan = Wlan::open();
            let mut endpoint: Option<IAudioEndpointVolume> = None;
            let mut media = MediaSessions::new();
            let mut slow_at = Instant::now() - Duration::from_secs(60);
            let mut media_at = slow_at;
            let mut current = Status::default();
            let mut wait = Duration::ZERO; // First poll right away.
            loop {
                let cmd = match rx.recv_timeout(std::mem::replace(&mut wait, Duration::from_millis(400))) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                let mut next = current.clone();
                match cmd {
                    Some(Cmd::SetOutput(id)) => {
                        set_default_output(&id);
                        slow_at = Instant::now() - Duration::from_secs(60); // Re-read devices now.
                    }
                    Some(c @ (Cmd::PlayPause | Cmd::Next | Cmd::Previous | Cmd::SeekMedia(_))) => {
                        media.control(&c);
                        media_at = Instant::now() - Duration::from_secs(60);
                    }
                    Some(Cmd::SelectPlayer(app)) => {
                        media.selected_app = Some(app.clone());
                        next.selected_player = Some(app);
                        media_at = Instant::now() - Duration::from_secs(60);
                    }
                    Some(Cmd::ConnectWifi(profile)) => {
                        connect_wifi_profile(&profile);
                        slow_at = Instant::now() - Duration::from_secs(60);
                    }
                    Some(Cmd::RefreshWifi) => {
                        slow_at = Instant::now() - Duration::from_secs(60);
                    }
                    Some(c) => {
                        if let Some(ep) = endpoint.as_ref() {
                            apply_audio(ep, c);
                        }
                    }
                    None => {}
                }
                // The default output device can change: look it up again with the slow poll.
                if slow_at.elapsed() >= Duration::from_secs(3) {
                    slow_at = Instant::now();
                    endpoint = default_endpoint();
                    next.outputs = list_outputs();
                    next.network = query_network(wlan.as_mut());
                    next.wifi_networks = wlan.as_mut().map(|w| w.available_networks()).unwrap_or_default();
                    next.bluetooth = query_bluetooth();
                    next.battery = query_battery();
                }
                if media_at.elapsed() >= Duration::from_secs(1) {
                    media_at = Instant::now();
                    let (cur, all) = media.poll_all();
                    next.media = cur;
                    next.media_players = all;
                    next.selected_player = media.selected_app.clone();
                    next.tray = query_tray();
                    if let Some((unread, dnd)) = query_notifications() {
                        next.unread = unread;
                        next.dnd = dnd;
                    }
                }
                next.volume = endpoint.as_ref().and_then(read_volume);
                if next.volume.is_none() {
                    endpoint = None;
                }
                if next != current {
                    current = next;
                    *shared.lock() = current.clone();
                    on_change();
                }
            }
        });
        Self { status, cmds }
    }

    pub fn snapshot(&self) -> Status {
        self.status.lock().clone()
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
    }
}

fn query_tray() -> Vec<chickenwm_core::ipc::TrayIcon> {
    use chickenwm_core::ipc::{send_ipc_command_blocking, IpcCommand, IpcResponse, PIPE_NAME_DAEMON};
    match send_ipc_command_blocking(PIPE_NAME_DAEMON, &IpcCommand::QueryTray) {
        Ok(IpcResponse::Tray(icons)) => icons.into_iter().filter(|i| !i.hidden && i.icon != 0).collect(),
        _ => Vec::new(),
    }
}

fn query_notifications() -> Option<(u32, bool)> {
    use chickenwm_core::ipc::{send_ipc_command_blocking, IpcCommand, IpcResponse, PIPE_NAME_NOTIFY};
    match send_ipc_command_blocking(PIPE_NAME_NOTIFY, &IpcCommand::QueryNotificationStatus) {
        Ok(IpcResponse::NotificationStatus { unread, dnd }) => Some((unread, dnd)),
        _ => None,
    }
}

/// Opens a `ms-settings:` page (or any URI) through the shell, without needing Explorer running.
///
/// Runs on a thread of its own: `ShellExecute` runs the URI's COM handler and can take seconds
/// (or wait on a shell that isn't there), and on the bar's UI thread that froze the whole bar.
pub fn open_uri(uri: &str) {
    let uri = uri.to_owned();
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let verb = HSTRING::from("open");
        let file = HSTRING::from(uri.as_str());
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            // Finish the launch before this thread exits; no error dialogs from the shell.
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let started = Instant::now();
        let result = ShellExecuteExW(&mut info);
        let took = started.elapsed();
        match result {
            Err(e) => tracing::warn!("Opening {uri} failed: {e}"),
            Ok(()) if took > Duration::from_secs(2) => {
                tracing::warn!("Opening {uri} took {:.1} s", took.as_secs_f32())
            }
            Ok(()) => {}
        }
        CoUninitialize();
    });
}

fn default_endpoint() -> Option<IAudioEndpointVolume> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()
    }
}

fn read_volume(ep: &IAudioEndpointVolume) -> Option<Volume> {
    unsafe {
        let level = ep.GetMasterVolumeLevelScalar().ok()?;
        let muted = ep.GetMute().ok()?.as_bool();
        Some(Volume { level, muted })
    }
}

fn apply_audio(ep: &IAudioEndpointVolume, cmd: Cmd) {
    unsafe {
        match cmd {
            Cmd::ToggleMute => {
                if let Ok(muted) = ep.GetMute() {
                    let _ = ep.SetMute(!muted.as_bool(), std::ptr::null());
                }
            }
            Cmd::Adjust(delta) => {
                if let Ok(level) = ep.GetMasterVolumeLevelScalar() {
                    let _ = ep.SetMasterVolumeLevelScalar((level + delta).clamp(0.0, 1.0), std::ptr::null());
                    // Turning the volume up is a request to hear something.
                    if delta > 0.0 {
                        let _ = ep.SetMute(false, std::ptr::null());
                    }
                }
            }
            Cmd::SetVolume(level) => {
                let _ = ep.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), std::ptr::null());
            }
            Cmd::SetOutput(_)
            | Cmd::PlayPause
            | Cmd::Next
            | Cmd::Previous
            | Cmd::SeekMedia(_)
            | Cmd::SelectPlayer(_)
            | Cmd::ConnectWifi(_)
            | Cmd::RefreshWifi => {}
        }
    }
}

/// Active playback devices with their friendly names; the default one is marked.
fn list_outputs() -> Vec<Output> {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Media::Audio::DEVICE_STATE_ACTIVE;
    use windows::Win32::System::Com::STGM_READ;
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    let mut out = Vec::new();
    unsafe {
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return out;
        };
        let default_id = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .and_then(|d| d.GetId())
            .map(|p| {
                let s = p.to_string().unwrap_or_default();
                windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
                s
            })
            .unwrap_or_default();
        let Ok(devices) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return out;
        };
        let count = devices.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(device) = devices.Item(i) else { continue };
            let Ok(id_ptr) = device.GetId() else { continue };
            let id = id_ptr.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(id_ptr.0 as _));
            let name = device
                .OpenPropertyStore(STGM_READ)
                .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
                .and_then(|value| PropVariantToStringAlloc(&value))
                .map(|p| {
                    let s = p.to_string().unwrap_or_default();
                    windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
                    s
                })
                .unwrap_or_else(|_| "Output".to_owned());
            out.push(Output { default: id == default_id, id, name });
        }
    }
    out
}

/// `IPolicyConfig`: the (undocumented, but long-stable) interface Windows' own sound settings
/// use to change the default device. Only `SetDefaultEndpoint` is called; the methods before it
/// are declared so the vtable lines up.
mod policy_config {
    #![allow(non_snake_case)]
    use windows::core::{HRESULT, HSTRING, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    type Ptr = *mut core::ffi::c_void;

    #[windows_core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]
    unsafe trait IPolicyConfig: windows::core::IUnknown {
        fn GetMixFormat(&self, id: PCWSTR, format: *mut Ptr) -> HRESULT;
        fn GetDeviceFormat(&self, id: PCWSTR, default: i32, format: *mut Ptr) -> HRESULT;
        fn ResetDeviceFormat(&self, id: PCWSTR) -> HRESULT;
        fn SetDeviceFormat(&self, id: PCWSTR, a: Ptr, b: Ptr) -> HRESULT;
        fn GetProcessingPeriod(&self, id: PCWSTR, default: i32, a: *mut i64, b: *mut i64) -> HRESULT;
        fn SetProcessingPeriod(&self, id: PCWSTR, a: *mut i64) -> HRESULT;
        fn GetShareMode(&self, id: PCWSTR, mode: Ptr) -> HRESULT;
        fn SetShareMode(&self, id: PCWSTR, mode: Ptr) -> HRESULT;
        fn GetPropertyValue(&self, id: PCWSTR, key: *const core::ffi::c_void, value: Ptr) -> HRESULT;
        fn SetPropertyValue(&self, id: PCWSTR, key: *const core::ffi::c_void, value: Ptr) -> HRESULT;
        fn SetDefaultEndpoint(&self, id: PCWSTR, role: i32) -> HRESULT;
    }

    /// Makes `id` the default output for every role (console, multimedia, communications).
    pub fn set_default_output(id: &str) {
        const CLSID_POLICY_CONFIG: windows::core::GUID = windows::core::GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
        unsafe {
            let Ok(policy) = CoCreateInstance::<_, IPolicyConfig>(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL) else {
                tracing::warn!("roobar: IPolicyConfig unavailable; can't switch output");
                return;
            };
            let id = HSTRING::from(id);
            for role in 0..3 {
                let _ = policy.SetDefaultEndpoint(PCWSTR(id.as_ptr()), role);
            }
        }
    }
}
use policy_config::set_default_output;

/// The system media session (`GlobalSystemMediaTransportControlsSessionManager`): whatever is
/// shown in Windows' own media flyout, plus per-player selection across all sessions.
struct MediaSessions {
    manager: Option<windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager>,
    selected_app: Option<String>,
}

impl MediaSessions {
    fn new() -> Self {
        use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as Manager;
        let manager = Manager::RequestAsync().and_then(|op| op.get()).ok();
        Self {
            manager,
            selected_app: None,
        }
    }

    fn active_session(&self) -> Option<windows::Media::Control::GlobalSystemMediaTransportControlsSession> {
        let mgr = self.manager.as_ref()?;
        if let Some(target) = &self.selected_app {
            if let Ok(sessions) = mgr.GetSessions() {
                for s in sessions {
                    if s.SourceAppUserModelId()
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        == *target
                    {
                        return Some(s);
                    }
                }
            }
        }
        mgr.GetCurrentSession().ok()
    }

    fn read_session(session: &windows::Media::Control::GlobalSystemMediaTransportControlsSession) -> Option<Media> {
        use windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status;
        let props = session.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
        let title = props.Title().map(|s| s.to_string()).unwrap_or_default();
        if title.is_empty() {
            return None;
        }
        let artist = props.Artist().map(|s| s.to_string()).unwrap_or_default();
        let album = props.AlbumTitle().map(|s| s.to_string()).unwrap_or_default();
        let playing = session
            .GetPlaybackInfo()
            .and_then(|i| i.PlaybackStatus())
            .is_ok_and(|s| s == Status::Playing);
        let app = session.SourceAppUserModelId().map(|s| s.to_string()).unwrap_or_default();
        let (position_secs, duration_secs) = session
            .GetTimelineProperties()
            .ok()
            .map(|tl| {
                let start = tl.StartTime().map(|t| t.Duration).unwrap_or(0);
                let end = tl.EndTime().map(|t| t.Duration).unwrap_or(0);
                let pos = tl.Position().map(|t| t.Duration).unwrap_or(0);
                let dur_s = ((end - start).max(0) as f64 / 10_000_000.0) as f32;
                let pos_s = ((pos - start).max(0) as f64 / 10_000_000.0) as f32;
                (pos_s.clamp(0.0, dur_s.max(0.0)), dur_s.max(0.0))
            })
            .unwrap_or((0.0, 0.0));
        Some(Media {
            title,
            artist,
            album,
            playing,
            app,
            position_secs,
            duration_secs,
        })
    }

    fn poll_all(&mut self) -> (Option<Media>, Vec<Media>) {
        let mut all = Vec::new();
        if let Some(mgr) = &self.manager {
            if let Ok(sessions) = mgr.GetSessions() {
                for s in sessions {
                    if let Some(m) = Self::read_session(&s) {
                        all.push(m);
                    }
                }
            }
        }
        if let Some(sel) = &self.selected_app {
            if !all.iter().any(|m| &m.app == sel) {
                self.selected_app = None;
            }
        }
        let current = self
            .active_session()
            .and_then(|s| Self::read_session(&s))
            .or_else(|| all.first().cloned());
        (current, all)
    }

    #[cfg(test)]
    fn current(&mut self) -> Option<Media> {
        self.poll_all().0
    }

    fn control(&mut self, cmd: &Cmd) {
        let Some(session) = self.active_session() else { return };
        let op = match cmd {
            Cmd::PlayPause => session.TryTogglePlayPauseAsync(),
            Cmd::Next => session.TrySkipNextAsync(),
            Cmd::Previous => session.TrySkipPreviousAsync(),
            Cmd::SeekMedia(secs) => {
                let ticks = (secs.max(0.0) as f64 * 10_000_000.0) as i64;
                session.TryChangePlaybackPositionAsync(ticks)
            }
            _ => return,
        };
        let _ = op.and_then(|op| op.get());
    }
}

unsafe fn extract_adapter_ipv4(a: &IP_ADAPTER_ADDRESSES_LH) -> String {
    let mut u = a.FirstUnicastAddress;
    while !u.is_null() {
        let addr = unsafe { (*u).Address.lpSockaddr };
        if !addr.is_null() && unsafe { (*addr).sa_family.0 } == 2 {
            let d = unsafe { &(*addr).sa_data };
            return format!(
                "{}.{}.{}.{}",
                d[2] as u8, d[3] as u8, d[4] as u8, d[5] as u8
            );
        }
        u = unsafe { (*u).Next };
    }
    String::new()
}

/// Wired connections win over Wi-Fi; only adapters that are up and have a gateway count, which
/// leaves out virtual switches and disconnected ports.
fn query_network(wlan: Option<&mut Wlan>) -> Network {
    let mut wifi_info: Option<(u32, String)> = None;
    unsafe {
        let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size = 16 * 1024u32;
        let mut buf: Vec<u64> = Vec::new();
        for _ in 0..3 {
            buf = vec![0u64; (size as usize).div_ceil(8)];
            let ret = GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                flags,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            );
            if ret == 0 {
                break;
            }
            if ret != 111 {
                // Anything but ERROR_BUFFER_OVERFLOW
                buf.clear();
                break;
            }
        }
        if !buf.is_empty() {
            let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
            while !cur.is_null() {
                let a = &*cur;
                let up = a.OperStatus.0 == IF_OPER_STATUS_UP && !a.FirstGatewayAddress.is_null();
                let speed_mbps = (a.ReceiveLinkSpeed / 1_000_000).min(100_000) as u32;
                let ip = extract_adapter_ipv4(a);
                if up && a.IfType == IF_TYPE_ETHERNET {
                    let adapter = a.FriendlyName.to_string().unwrap_or_default();
                    return Network::Ethernet {
                        adapter,
                        speed_mbps,
                        ip,
                    };
                }
                if up && a.IfType == IF_TYPE_WIFI {
                    wifi_info = Some((speed_mbps, ip));
                }
                cur = a.Next;
            }
        }
    }
    if let Some((speed_mbps, ip)) = wifi_info {
        if let Some((ssid, signal)) = wlan.and_then(|w| w.current()) {
            return Network::Wifi {
                ssid,
                signal,
                speed_mbps,
                ip,
            };
        }
        return Network::Wifi {
            ssid: String::new(),
            signal: 0,
            speed_mbps,
            ip,
        };
    }
    Network::Disconnected
}

fn connect_wifi_profile(profile: &str) {
    if profile.trim().is_empty() {
        open_uri("ms-settings:network-wifi");
        return;
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let _ = std::process::Command::new("netsh")
        .args(["wlan", "connect", &format!("name={profile}")])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

struct Wlan(HANDLE);

impl Wlan {
    fn open() -> Option<Self> {
        let mut version = 0u32;
        let mut handle = HANDLE::default();
        let ok = unsafe { WlanOpenHandle(2, None, &mut version, &mut handle) } == 0;
        ok.then_some(Self(handle))
    }

    /// SSID and signal quality (0..=100) of the first connected Wi-Fi interface.
    fn current(&mut self) -> Option<(String, u32)> {
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(self.0, None, &mut list) != 0 || list.is_null() {
                return None;
            }
            let count = (*list).dwNumberOfItems as usize;
            let infos = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count);
            let mut found = None;
            for info in infos {
                if info.isState != wlan_interface_state_connected {
                    continue;
                }
                let mut size = 0u32;
                let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
                let ret = WlanQueryInterface(
                    self.0,
                    &info.InterfaceGuid,
                    wlan_intf_opcode_current_connection,
                    None,
                    &mut size,
                    &mut data,
                    None,
                );
                if ret != 0 || data.is_null() {
                    continue;
                }
                let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                let ssid = &attrs.wlanAssociationAttributes.dot11Ssid;
                let len = (ssid.uSSIDLength as usize).min(ssid.ucSSID.len());
                let name = String::from_utf8_lossy(&ssid.ucSSID[..len]).into_owned();
                found = Some((name, attrs.wlanAssociationAttributes.wlanSignalQuality));
                WlanFreeMemory(data);
                break;
            }
            WlanFreeMemory(list as *const core::ffi::c_void);
            found
        }
    }

    /// Visible Wi-Fi networks sorted by connected status then signal strength.
    fn available_networks(&mut self) -> Vec<WifiNetwork> {
        let mut results: Vec<WifiNetwork> = Vec::new();
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(self.0, None, &mut list) != 0 || list.is_null() {
                return results;
            }
            let count = (*list).dwNumberOfItems as usize;
            let infos = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count);
            for info in infos {
                let mut net_list: *mut WLAN_AVAILABLE_NETWORK_LIST = std::ptr::null_mut();
                if WlanGetAvailableNetworkList(self.0, &info.InterfaceGuid, 0, None, &mut net_list) == 0
                    && !net_list.is_null()
                {
                    let n_count = (*net_list).dwNumberOfItems as usize;
                    let nets = std::slice::from_raw_parts((*net_list).Network.as_ptr(), n_count);
                    for net in nets {
                        let ssid_len = (net.dot11Ssid.uSSIDLength as usize).min(net.dot11Ssid.ucSSID.len());
                        if ssid_len == 0 {
                            continue;
                        }
                        let ssid = String::from_utf8_lossy(&net.dot11Ssid.ucSSID[..ssid_len]).into_owned();
                        if ssid.trim().is_empty() {
                            continue;
                        }
                        let p_len = net
                            .strProfileName
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(net.strProfileName.len());
                        let profile = String::from_utf16_lossy(&net.strProfileName[..p_len]);
                        let connected = (net.dwFlags & 1) != 0;
                        let secured = net.bSecurityEnabled.as_bool();
                        let signal = net.wlanSignalQuality;
                        if let Some(existing) = results.iter_mut().find(|r| r.ssid == ssid) {
                            existing.connected |= connected;
                            existing.signal = existing.signal.max(signal);
                            if existing.profile.is_empty() && !profile.is_empty() {
                                existing.profile = profile;
                            }
                        } else {
                            results.push(WifiNetwork {
                                ssid,
                                signal,
                                connected,
                                secured,
                                profile,
                            });
                        }
                    }
                    WlanFreeMemory(net_list as *const core::ffi::c_void);
                }
            }
            WlanFreeMemory(list as *const core::ffi::c_void);
        }
        results.sort_by(|a, b| b.connected.cmp(&a.connected).then_with(|| b.signal.cmp(&a.signal)));
        results
    }
}

impl Drop for Wlan {
    fn drop(&mut self) {
        unsafe {
            WlanCloseHandle(self.0, None);
        }
    }
}

/// Radio state from WinRT (`Windows.Devices.Radios`), connected devices from the classic
/// Bluetooth API. Without WinRT access a present radio is assumed to be on.
fn query_bluetooth() -> Bluetooth {
    let present = unsafe {
        let params = BLUETOOTH_FIND_RADIO_PARAMS { dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32 };
        let mut radio = HANDLE::default();
        match BluetoothFindFirstRadio(&params, &mut radio) {
            Ok(find) => {
                let _ = CloseHandle(radio);
                let _ = BluetoothFindRadioClose(find);
                true
            }
            Err(_) => false,
        }
    };
    let state = radio_state();
    if !present && state.is_none() {
        return Bluetooth::Unavailable;
    }
    if state == Some(false) {
        return Bluetooth::Off;
    }
    Bluetooth::On { devices: connected_devices() }
}

/// `Some(true)` = on, `Some(false)` = off, `None` = no Bluetooth radio or no access.
fn radio_state() -> Option<bool> {
    use windows::Devices::Radios::{Radio, RadioKind, RadioState};
    let radios = Radio::GetRadiosAsync().ok()?.get().ok()?;
    for radio in radios {
        if radio.Kind().ok()? == RadioKind::Bluetooth {
            return Some(radio.State().ok()? == RadioState::On);
        }
    }
    None
}

fn connected_devices() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let params = BLUETOOTH_DEVICE_SEARCH_PARAMS {
            dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
            fReturnAuthenticated: true.into(),
            fReturnRemembered: true.into(),
            fReturnUnknown: false.into(),
            fReturnConnected: true.into(),
            fIssueInquiry: false.into(),
            cTimeoutMultiplier: 0,
            hRadio: HANDLE::default(),
        };
        let mut info = BLUETOOTH_DEVICE_INFO {
            dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
            ..Default::default()
        };
        let Ok(find) = BluetoothFindFirstDevice(&params, &mut info) else {
            return out;
        };
        loop {
            if info.fConnected.as_bool() {
                let len = info.szName.iter().position(|&c| c == 0).unwrap_or(info.szName.len());
                out.push(String::from_utf16_lossy(&info.szName[..len]));
            }
            info = BLUETOOTH_DEVICE_INFO {
                dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
                ..Default::default()
            };
            if BluetoothFindNextDevice(find, &mut info).is_err() {
                break;
            }
        }
        let _ = BluetoothFindDeviceClose(find);
    }
    out
}

/// Queries system battery status with strict hardware guards.
/// Returns `None` on desktop PCs, virtual machines, or devices without a functional battery.
pub fn query_battery() -> Option<Battery> {
    unsafe {
        let mut sps = windows::Win32::System::Power::SYSTEM_POWER_STATUS::default();
        if windows::Win32::System::Power::GetSystemPowerStatus(&mut sps).is_err() {
            return None;
        }

        // Critical Hardware Guard:
        // BatteryFlag:
        //   128 = No system battery (desktop PC, VM)
        //   255 = Unable to read battery flag
        // BatteryLifePercent:
        //   255 = Unknown status (desktop PC without battery)
        if sps.BatteryFlag & 128 != 0 || sps.BatteryFlag == 255 || sps.BatteryLifePercent == 255 {
            return None;
        }

        let percent = sps.BatteryLifePercent.min(100);
        let charging = (sps.BatteryFlag & 8) != 0;
        let ac_online = sps.ACLineStatus == 1;
        let remaining_seconds = if sps.BatteryLifeTime != 0xFFFFFFFF && sps.BatteryLifeTime > 0 {
            Some(sps.BatteryLifeTime)
        } else {
            None
        };

        Some(Battery {
            percent,
            charging,
            ac_online,
            remaining_seconds,
        })
    }
}

#[cfg(test)]
mod tests {
    /// Reads the real machine's state; run with `cargo test -p roobar -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_status() {
        unsafe {
            let _ = super::CoInitializeEx(None, super::COINIT_MULTITHREADED);
        }
        println!("network: {:?}", super::query_network(super::Wlan::open().as_mut()));
        println!("volume: {:?}", super::default_endpoint().as_ref().and_then(super::read_volume));
        println!("bluetooth: {:?} (radio {:?})", super::query_bluetooth(), super::radio_state());
        println!("outputs: {:?}", super::list_outputs());
        println!("media: {:?}", super::MediaSessions::new().current());
    }
}
