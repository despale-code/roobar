//! Top processes by CPU and memory, for the stats popover. Processes with the same executable
//! name are added together (a browser's many processes count as one row).

use std::collections::HashMap;
use std::time::Instant;
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

#[derive(Clone, Debug, PartialEq)]
pub struct ProcRow {
    pub name: String,
    /// Percent of all cores.
    pub cpu: f32,
    pub mem_mb: f32,
    /// Processes folded into this row.
    pub count: u32,
}

#[derive(Default)]
pub struct ProcSampler {
    /// pid -> (cpu time in 100 ns units) from the previous sample.
    last: HashMap<u32, u64>,
    last_at: Option<Instant>,
}

fn ft(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

impl ProcSampler {
    /// Takes a sample. CPU figures are relative to the previous call (zero on the first).
    pub fn sample(&mut self) -> Vec<ProcRow> {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
        let elapsed = self.last_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        let mut rows: HashMap<String, ProcRow> = HashMap::new();
        let mut now_times = HashMap::new();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return Vec::new();
            };
            let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut ok = Process32FirstW(snap, &mut entry).is_ok();
            while ok {
                let pid = entry.th32ProcessID;
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if pid != 0 {
                    if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                        let (mut c, mut e, mut k, mut u) = Default::default();
                        let cpu_time = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_ok().then(|| ft(k) + ft(u));
                        let mut mem = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
                        let mem_mb = if K32GetProcessMemoryInfo(h, &mut mem, mem.cb).as_bool() {
                            mem.WorkingSetSize as f32 / (1024.0 * 1024.0)
                        } else {
                            0.0
                        };
                        let _ = CloseHandle(h);
                        let cpu = match (cpu_time, self.last.get(&pid)) {
                            (Some(now), Some(&prev)) if elapsed > 0.0 => {
                                (now.saturating_sub(prev) as f64 / 1e7 / elapsed / cores * 100.0) as f32
                            }
                            _ => 0.0,
                        };
                        if let Some(t) = cpu_time {
                            now_times.insert(pid, t);
                        }
                        let row = rows.entry(name.to_lowercase()).or_insert_with(|| ProcRow {
                            name: name.trim_end_matches(".exe").trim_end_matches(".EXE").to_owned(),
                            cpu: 0.0,
                            mem_mb: 0.0,
                            count: 0,
                        });
                        row.cpu += cpu;
                        row.mem_mb += mem_mb;
                        row.count += 1;
                    }
                }
                ok = Process32NextW(snap, &mut entry).is_ok();
            }
            let _ = CloseHandle(snap);
        }
        self.last = now_times;
        self.last_at = Some(Instant::now());
        rows.into_values().collect()
    }
}

/// The `n` heaviest rows by `key`, heaviest first.
pub fn top(rows: &[ProcRow], n: usize, key: impl Fn(&ProcRow) -> f32) -> Vec<ProcRow> {
    let mut v = rows.to_vec();
    v.sort_by(|a, b| key(b).total_cmp(&key(a)));
    v.truncate(n);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_this_machine() {
        let mut s = ProcSampler::default();
        let first = s.sample();
        assert!(first.iter().any(|r| r.mem_mb > 0.0), "no process memory read");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let rows = s.sample();
        let total: f32 = rows.iter().map(|r| r.cpu).sum();
        assert!((0.0..=110.0).contains(&total), "cpu total {total}");
        let by_mem = top(&rows, 5, |r| r.mem_mb);
        assert!(by_mem.len() <= 5 && by_mem.windows(2).all(|w| w[0].mem_mb >= w[1].mem_mb));
    }
}
