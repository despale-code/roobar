//! The bar's widget library and its configuration.
//!
//! The right side of the bar is a list of widgets read from `roobar.css` (hot-reloaded):
//!
//! ```css
//! :root {
//!     /* Left to right. Any widget can be listed, in any order, more than once. */
//!     --modules-right: media stats tray network volume bluetooth clock notifications menu;
//!     /* These share one pill, in this order, where the first of them sits; `none` turns it off. */
//!     --modules-group: network volume bluetooth clock notifications;
//!     --tray-visible: 4;                   /* tray icons before the overflow chevron */
//!     --clock-format: %a %Y-%m-%d  %H:%M;   /* strftime-style; add :%S for seconds */
//!     --refresh-rate-dwm: true;             /* "165Hz · udwm" when the DWM hook is active */
//!     --network-label: false;               /* Wi-Fi name / "ethernet" next to the icon */
//!     --bluetooth-label: false;             /* connected device name next to the icon */
//!     --volume-step: 5;                     /* percent per scroll notch */
//! }
//! ```

use chickenwm_core::css::CssStyleSheet;
use windows::Win32::Foundation::SYSTEMTIME;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Widget {
    /// CPU and RAM in one island.
    Stats,
    Cpu,
    Memory,
    /// Monitor refresh rate (and the DWM hook state).
    RefreshRate,
    Network,
    Volume,
    Bluetooth,
    /// Now playing (Windows media session); hidden when nothing is.
    Media,
    /// System tray icons (hosted by rooster), with an overflow chevron.
    Tray,
    /// Bell with the unread count; opens the notification center.
    Notifications,
    Files,
    Settings,
    Clock,
    Power,
    /// Hamburger button: File Explorer, Settings, About this PC, About ChickenWM, Shut down.
    Menu,
}

impl Widget {
    /// Every widget with the name it goes by in `--modules-right`.
    pub const ALL: [(&'static str, Widget); 15] = [
        ("stats", Widget::Stats),
        ("cpu", Widget::Cpu),
        ("memory", Widget::Memory),
        ("refresh-rate", Widget::RefreshRate),
        ("network", Widget::Network),
        ("volume", Widget::Volume),
        ("bluetooth", Widget::Bluetooth),
        ("media", Widget::Media),
        ("tray", Widget::Tray),
        ("notifications", Widget::Notifications),
        ("files", Widget::Files),
        ("settings", Widget::Settings),
        ("clock", Widget::Clock),
        ("power", Widget::Power),
        ("menu", Widget::Menu),
    ];

    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        // A few aliases people will reach for.
        let name = match name.as_str() {
            "ram" | "mem" => "memory",
            "hz" | "refresh" => "refresh-rate",
            "wifi" | "ethernet" | "net" => "network",
            "audio" | "sound" => "volume",
            "bt" => "bluetooth",
            "music" | "player" | "now-playing" => "media",
            "systray" | "tray-icons" => "tray",
            "notification" | "bell" | "notify" => "notifications",
            "time" | "date" => "clock",
            "hamburger" | "start" => "menu",
            other => other,
        };
        Self::ALL.iter().find(|(n, _)| *n == name).map(|(_, w)| *w)
    }

    pub fn name(self) -> &'static str {
        Self::ALL.iter().find(|(_, w)| *w == self).map(|(n, _)| *n).unwrap_or("?")
    }
}

/// The default right side: the refresh-rate widget is in the library but not shown.
pub const DEFAULT_RIGHT: [Widget; 9] = [
    Widget::Media,
    Widget::Stats,
    Widget::Tray,
    Widget::Network,
    Widget::Volume,
    Widget::Bluetooth,
    Widget::Clock,
    Widget::Notifications,
    Widget::Menu,
];

/// The default `--modules-group`: these share one pill, in this order.
pub const DEFAULT_GROUP: [Widget; 5] =
    [Widget::Network, Widget::Volume, Widget::Bluetooth, Widget::Clock, Widget::Notifications];

pub const DEFAULT_CLOCK_FORMAT: &str = "%a %Y-%m-%d  %H:%M";

/// Strips one layer of matching quotes, so `"…"` and `'…'` values work too.
fn unquote(v: &str) -> &str {
    let v = v.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return &v[1..v.len() - 1];
        }
    }
    v
}

/// Parses a widget list (`stats, network volume`); unknown names are skipped.
pub fn parse_list(value: &str) -> Vec<Widget> {
    unquote(value)
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            let w = Widget::parse(s);
            if w.is_none() {
                tracing::warn!("roobar: unknown widget '{s}' in --modules-right");
            }
            w
        })
        .collect()
}

/// `--modules-right`, left to right; the default list when it isn't set.
pub fn right_modules(css: &CssStyleSheet) -> Vec<Widget> {
    match css.custom_properties.get("--modules-right") {
        Some(v) => parse_list(v),
        None => DEFAULT_RIGHT.to_vec(),
    }
}

/// One spot on the right side: a widget in its own pill, or several sharing one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Slot {
    Single(Widget),
    Group(Vec<Widget>),
}

/// The right side, left to right. Widgets named in `--modules-group` (`none` turns it off) and
/// present in `--modules-right` share one pill, in the group's order, where the first of them
/// sits in `--modules-right`.
pub fn right_layout(css: &CssStyleSheet) -> Vec<Slot> {
    let group = match css.custom_properties.get("--modules-group") {
        Some(v) if unquote(v).eq_ignore_ascii_case("none") => Vec::new(),
        Some(v) => parse_list(v),
        None => DEFAULT_GROUP.to_vec(),
    };
    layout_with_group(&right_modules(css), &group)
}

fn layout_with_group(modules: &[Widget], group: &[Widget]) -> Vec<Slot> {
    let mut members: Vec<Widget> = Vec::new();
    for w in group {
        if modules.contains(w) && !members.contains(w) {
            members.push(*w);
        }
    }
    if members.len() < 2 {
        return modules.iter().map(|w| Slot::Single(*w)).collect();
    }
    let mut out = Vec::new();
    let mut placed = false;
    for w in modules {
        if !members.contains(w) {
            out.push(Slot::Single(*w));
        } else if !placed {
            out.push(Slot::Group(members.clone()));
            placed = true;
        }
    }
    out
}

pub fn css_string(css: &CssStyleSheet, name: &str) -> Option<String> {
    css.custom_properties.get(name).map(|v| unquote(v).to_owned())
}

pub fn css_flag(css: &CssStyleSheet, name: &str, default: bool) -> bool {
    match css_string(css, name).map(|v| v.to_ascii_lowercase()) {
        Some(v) if matches!(v.as_str(), "true" | "yes" | "on" | "1") => true,
        Some(v) if matches!(v.as_str(), "false" | "no" | "off" | "0" | "none") => false,
        _ => default,
    }
}

/// strftime-style clock: `%a %A %b %B %d %e %m %y %Y %H %I %M %S %p %%`.
pub fn format_clock(fmt: &str, t: &SYSTEMTIME) -> String {
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
        "November", "December",
    ];
    let day = DAYS[(t.wDayOfWeek as usize).min(6)];
    let month = MONTHS[(t.wMonth.clamp(1, 12) - 1) as usize];
    let hour12 = match t.wHour % 12 {
        0 => 12,
        h => h,
    };
    let mut out = String::with_capacity(fmt.len() + 8);
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('a') => out.push_str(&day[..3]),
            Some('A') => out.push_str(day),
            Some('b') => out.push_str(&month[..3]),
            Some('B') => out.push_str(month),
            Some('d') => out.push_str(&format!("{:02}", t.wDay)),
            Some('e') => out.push_str(&t.wDay.to_string()),
            Some('m') => out.push_str(&format!("{:02}", t.wMonth)),
            Some('y') => out.push_str(&format!("{:02}", t.wYear % 100)),
            Some('Y') => out.push_str(&t.wYear.to_string()),
            Some('H') => out.push_str(&format!("{:02}", t.wHour)),
            Some('I') => out.push_str(&format!("{hour12:02}")),
            Some('M') => out.push_str(&format!("{:02}", t.wMinute)),
            Some('S') => out.push_str(&format!("{:02}", t.wSecond)),
            Some('p') => out.push_str(if t.wHour < 12 { "AM" } else { "PM" }),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 31,
    }
}

/// 0 = Monday … 6 = Sunday.
pub fn weekday_monday0(year: i32, month: u32, day: u32) -> u32 {
    // Sakamoto's method gives 0 = Sunday.
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month < 3 { year - 1 } else { year };
    let sunday0 = (y + y / 4 - y / 100 + y / 400 + T[(month - 1) as usize] + day as i32).rem_euclid(7) as u32;
    (sunday0 + 6) % 7
}

/// `(year, month)` moved by `delta` months.
pub fn add_months(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let index = year * 12 + month as i32 - 1 + delta;
    (index.div_euclid(12), (index.rem_euclid(12) + 1) as u32)
}

pub const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November",
    "December",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_math() {
        assert_eq!(weekday_monday0(2026, 10, 18), 6); // a Sunday
        assert_eq!(weekday_monday0(2026, 10, 1), 3); // a Thursday
        assert_eq!(weekday_monday0(2000, 1, 1), 5); // a Saturday
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(add_months(2026, 1, -1), (2025, 12));
        assert_eq!(add_months(2026, 12, 1), (2027, 1));
        assert_eq!(add_months(2026, 10, 0), (2026, 10));
    }

    fn time() -> SYSTEMTIME {
        SYSTEMTIME { wYear: 2026, wMonth: 10, wDayOfWeek: 0, wDay: 8, wHour: 14, wMinute: 5, wSecond: 9, wMilliseconds: 0 }
    }

    #[test]
    fn clock_formats() {
        assert_eq!(format_clock(DEFAULT_CLOCK_FORMAT, &time()), "Sun 2026-10-08  14:05");
        assert_eq!(format_clock("%H:%M:%S", &time()), "14:05:09");
        assert_eq!(format_clock("%e %b, %I:%M %p", &time()), "8 Oct, 02:05 PM");
        assert_eq!(format_clock("100%% %q", &time()), "100% %q");
    }

    #[test]
    fn widget_lists() {
        assert_eq!(parse_list("\"stats, wifi  bt clock\""), vec![Widget::Stats, Widget::Network, Widget::Bluetooth, Widget::Clock]);
        assert_eq!(parse_list("nope power"), vec![Widget::Power]);
        for (name, w) in Widget::ALL {
            assert_eq!(Widget::parse(name), Some(w));
            assert_eq!(w.name(), name);
        }
        assert!(!DEFAULT_RIGHT.contains(&Widget::RefreshRate));
    }

    #[test]
    fn grouped_layout() {
        use Widget::*;
        let l = layout_with_group(&DEFAULT_RIGHT, &DEFAULT_GROUP);
        assert_eq!(l.len(), 5);
        assert_eq!(l[3], Slot::Group(vec![Network, Volume, Bluetooth, Clock, Notifications]));
        assert_eq!(l[4], Slot::Single(Menu));
        // Members missing from --modules-right drop out; fewer than two means no group.
        assert_eq!(
            layout_with_group(&[Clock, Power, Volume], &DEFAULT_GROUP),
            vec![Slot::Group(vec![Volume, Clock]), Slot::Single(Power)]
        );
        assert_eq!(layout_with_group(&[Clock, Power], &DEFAULT_GROUP), vec![Slot::Single(Clock), Slot::Single(Power)]);
    }
}

/// A mouse gesture on a widget, overridable per widget in `roobar.css`:
///
/// ```css
/// :root {
///     --clock-on-click: exec wt.exe;                   /* run a command line */
///     --volume-on-right: open ms-settings:sound;       /* a URI, file or folder */
///     --stats-on-middle: dispatch exec taskmgr.exe;    /* a ChickenWM dispatcher */
///     --media-on-scroll-up: none;                      /* do nothing */
/// }
/// ```
///
/// A gesture with no override keeps the widget's built-in behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    Click,
    Middle,
    Right,
    ScrollUp,
    ScrollDown,
}

impl Gesture {
    fn suffix(self) -> &'static str {
        match self {
            Gesture::Click => "click",
            Gesture::Middle => "middle",
            Gesture::Right => "right",
            Gesture::ScrollUp => "scroll-up",
            Gesture::ScrollDown => "scroll-down",
        }
    }
}

/// What the user set for a gesture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GestureAction {
    None,
    Exec(String),
    Open(String),
    Dispatch(String, String),
}

pub fn gesture_override(css: &CssStyleSheet, widget: Widget, gesture: Gesture) -> Option<GestureAction> {
    let raw = css_string(css, &format!("--{}-on-{}", widget.name(), gesture.suffix()))?;
    parse_gesture_action(&raw)
}

pub fn parse_gesture_action(raw: &str) -> Option<GestureAction> {
    let raw = raw.trim();
    let (head, rest) = raw.split_once(char::is_whitespace).map_or((raw, ""), |(h, r)| (h, r.trim()));
    Some(match head.to_ascii_lowercase().as_str() {
        "" | "default" => return None,
        "none" => GestureAction::None,
        "open" if !rest.is_empty() => GestureAction::Open(rest.to_owned()),
        "dispatch" if !rest.is_empty() => {
            let (d, a) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(d, a)| (d, a.trim()));
            GestureAction::Dispatch(d.to_owned(), a.to_owned())
        }
        "exec" if !rest.is_empty() => GestureAction::Exec(rest.to_owned()),
        _ => GestureAction::Exec(raw.to_owned()),
    })
}

#[cfg(test)]
mod gesture_tests {
    use super::*;

    #[test]
    fn parses_actions() {
        assert_eq!(parse_gesture_action("default"), None);
        assert_eq!(parse_gesture_action("none"), Some(GestureAction::None));
        assert_eq!(parse_gesture_action("exec wt.exe -d C:\\"), Some(GestureAction::Exec("wt.exe -d C:\\".into())));
        assert_eq!(parse_gesture_action("open ms-settings:sound"), Some(GestureAction::Open("ms-settings:sound".into())));
        assert_eq!(
            parse_gesture_action("dispatch workspace 3"),
            Some(GestureAction::Dispatch("workspace".into(), "3".into()))
        );
        assert_eq!(parse_gesture_action("notepad.exe"), Some(GestureAction::Exec("notepad.exe".into())));
    }
}
