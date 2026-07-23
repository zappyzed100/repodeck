//! Real-machine placement soak-test driver.
//!
//! Drives the running RepoDeck (started with `REPODECK_TEST_CONTROL=1`) through
//! hundreds of random workset switches over its test-control pipe, then — after
//! each switch settles — checks that every managed window actually ended up
//! where RepoDeck's own log says it placed it. This catches *drift*: a window
//! that RepoDeck placed into a cell but that later slid to a stale size /
//! position (the class of bug the in-process simulation can't see, because the
//! simulation trusts the placement it computed).
//!
//! For each switch it reads the new lines RepoDeck appended to its log, parses
//! the intended placement of every window (park cell / main restore / minimize),
//! then compares against the live `GetWindowRect` / DWM visible bounds. It also
//! asserts no two visible managed windows overlap and every test window is
//! accounted for (matched + managed), so an un-parked or unmatched window is a
//! failure too.
//!
//! Usage: `repodeck-simtest [--iters N] [--settle-ms M] [--out path.json]`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use repodeck::domain::placement::PixelRect;
use repodeck::persistence::config_store;
use repodeck::windowing::enumerate::{TopLevelWindow, enumerate_top_level_windows};
use repodeck::windowing::monitor::{MonitorInfo, enumerate_monitors};

use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, WriteFile,
};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsIconic};
use windows::core::HSTRING;

const TEST_CONTROL_PIPE: &str = r"\\.\pipe\RepoDeck.TestControl.v1";

fn main() {
    let opts = Options::parse();
    match run(&opts) {
        Ok(report) => {
            report.print();
            if let Some(out) = &opts.out {
                let _ = std::fs::write(out, report.to_json());
                eprintln!("[simtest] wrote {}", out.display());
            }
            std::process::exit(i32::from(!report.failures.is_empty()));
        }
        Err(e) => {
            eprintln!("[simtest] fatal: {e}");
            std::process::exit(2);
        }
    }
}

struct Options {
    iters: usize,
    warmup: usize,
    settle_ms: u64,
    confirm_budget_ms: u64,
    out: Option<PathBuf>,
}

impl Options {
    fn parse() -> Self {
        let mut iters = 300usize;
        let mut warmup = 30usize;
        let mut settle_ms = 1200u64;
        let mut confirm_budget_ms = 8000u64;
        let mut out = None;
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--iters" => {
                    iters = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(iters);
                    i += 1;
                }
                "--warmup" => {
                    warmup = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(warmup);
                    i += 1;
                }
                "--settle-ms" => {
                    settle_ms = args
                        .get(i + 1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(settle_ms);
                    i += 1;
                }
                "--confirm-ms" => {
                    confirm_budget_ms = args
                        .get(i + 1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(confirm_budget_ms);
                    i += 1;
                }
                "--out" => {
                    out = args.get(i + 1).map(PathBuf::from);
                    i += 1;
                }
                _ => {}
            }
            i += 1;
        }
        Self {
            iters,
            warmup,
            settle_ms,
            confirm_budget_ms,
            out,
        }
    }
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA not set");
    PathBuf::from(base).join("RepoDeck")
}

/// A workset that has at least one `SET-XX` test window matcher.
struct TestWorkset {
    id: String,
    name: String,
}

/// True if `title` contains one of the configured matcher substrings — so only
/// windows the current config actually manages are checked (stray windows and
/// worksets sliced out for a smaller size are ignored).
fn is_configured(title: &str, valid: &[String]) -> bool {
    valid.iter().any(|t| !t.is_empty() && title.contains(t.as_str()))
}

fn run(opts: &Options) -> Result<Report, String> {
    let dir = data_dir();
    let loaded = config_store::load(&dir)
        .map_err(|e| format!("load config: {e}"))?
        .ok_or("no config.json found")?;
    let cfg = loaded.config;

    let worksets: Vec<TestWorkset> = cfg
        .worksets
        .iter()
        .map(|w| TestWorkset {
            id: w.id.to_string(),
            name: w.name.clone(),
        })
        .collect();

    if worksets.len() < 2 {
        return Err(format!(
            "need >=2 SET-* worksets to switch between, found {}",
            worksets.len()
        ));
    }

    let main_ids: Vec<String> = cfg.main_monitor_ids.clone();
    // Every configured matcher's title substring identifies a managed test
    // window. (For real apps these are "repo07", "WS repo08", "BSET-03",
    // "ChatGPT" — no shared "SET-" prefix to rely on.)
    let valid_titles: Vec<String> = cfg
        .worksets
        .iter()
        .flat_map(|w| w.windows.iter())
        .filter_map(|m| m.matcher.title_contains.clone())
        .collect();

    eprintln!(
        "[simtest] {} worksets, {} iters, settle {}ms",
        worksets.len(),
        opts.iters,
        opts.settle_ms
    );

    let mut log = LogTail::open_newest(&dir.join("logs"))?;

    // Warm-up: switch through every workset a few times so each real app
    // (ChatGPT/VS Code/Brave) gets placed at least once and RepoDeck wins the
    // initial re-assert battle. First-time placement of a Chromium window that
    // re-asserts its own bounds can take >8s to settle; measuring across that
    // would be a false "drift". Results here are discarded.
    if opts.warmup > 0 {
        eprintln!("[simtest] warm-up: {} switches (no measurement)", opts.warmup);
        for i in 0..opts.warmup {
            let target = i % worksets.len();
            send_switch(&worksets[target].id)?;
            std::thread::sleep(Duration::from_millis(opts.settle_ms + 1500));
        }
        // Let the last warm-up switch's re-assert fully finish before measuring.
        std::thread::sleep(Duration::from_millis(4000));
    }
    log.seek_to_end(); // skip warm-up + pre-test history

    let mut rng = Rng::seeded();
    let mut report = Report::new(worksets.len());
    let mut current = usize::MAX; // none yet

    for iter in 0..opts.iters {
        // Pick a target different from the current one so every iteration is a
        // real switch (a same-target switch is a logged no-op with no placement).
        let mut target = rng.below(worksets.len());
        if worksets.len() > 1 {
            while target == current {
                target = rng.below(worksets.len());
            }
        }
        let tw = &worksets[target];

        send_switch(&tw.id)?;

        // Wait for the switch to settle, then pull this switch's log lines.
        std::thread::sleep(Duration::from_millis(opts.settle_ms));
        let slice = log.read_new_until(
            |s| s.contains("switch: completed to="),
            Duration::from_millis(2500),
        );

        let intents = parse_intents(&slice);
        if intents.is_empty() {
            // No-op (already current) or nothing logged — skip validation but
            // note it so a run that silently does nothing is visible.
            report.noops += 1;
            current = target;
            continue;
        }

        let monitors = enumerate_monitors().map_err(|e| format!("enumerate monitors: {e}"))?;
        let windows = enumerate_top_level_windows(std::process::id())
            .map_err(|e| format!("enumerate windows: {e}"))?;
        let test_windows: Vec<&TopLevelWindow> = windows
            .iter()
            .filter(|w| is_configured(&w.title, &valid_titles))
            .collect();

        let ctx = CheckCtx {
            iter,
            target_name: &tw.name,
            main_ids: &main_ids,
            monitors: &monitors,
        };
        let first = check(&ctx, &intents, &test_windows);

        if first.is_empty() {
            report.switches += 1;
            current = target;
            continue;
        }

        // Confirm pass, poll-until-clean: real Chromium apps (VS Code, Brave)
        // re-assert their own bounds for up to ~6.5s after a switch, so a
        // first-pass mismatch is usually a window still settling, not drift.
        // Re-measure every second up to `confirm_budget_ms`; a mismatch that
        // clears is transient, one that persists past the whole re-assert window
        // is a real placement failure (the SET-10-A class).
        let first_count = first.len();
        let mut fails = first;
        let mut waited = 0u64;
        while !fails.is_empty() && waited < opts.confirm_budget_ms {
            std::thread::sleep(Duration::from_millis(1000));
            waited += 1000;
            let mons = enumerate_monitors().map_err(|e| format!("enumerate monitors: {e}"))?;
            let wins = enumerate_top_level_windows(std::process::id())
                .map_err(|e| format!("enumerate windows: {e}"))?;
            let tws: Vec<&TopLevelWindow> = wins
                .iter()
                .filter(|w| is_configured(&w.title, &valid_titles))
                .collect();
            let c = CheckCtx {
                iter,
                target_name: &tw.name,
                main_ids: &main_ids,
                monitors: &mons,
            };
            fails = check(&c, &intents, &tws);
        }
        report.transient += first_count - fails.len();
        if !fails.is_empty() {
            eprintln!(
                "[simtest] iter {iter} target={}: {} CONFIRMED failures (after {}ms)",
                tw.name,
                fails.len(),
                waited
            );
            report.failures.extend(fails);
        }

        report.switches += 1;
        current = target;

        if (iter + 1) % 50 == 0 {
            eprintln!(
                "[simtest] {}/{} switches, {} failures so far",
                iter + 1,
                opts.iters,
                report.failures.len()
            );
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

struct CheckCtx<'a> {
    iter: usize,
    target_name: &'a str,
    main_ids: &'a [String],
    monitors: &'a [MonitorInfo],
}

#[derive(Clone)]
enum Intent {
    /// Parked / tiled: the window's *visible* bounds should equal this cell.
    Fill(PixelRect),
    /// Restored to a main monitor: maximized (or near-full) on this rect's monitor.
    Main(PixelRect),
    /// Overflow: should be minimized.
    Minimize,
}

fn check(ctx: &CheckCtx, intents: &[(isize, Intent)], test_windows: &[&TopLevelWindow]) -> Vec<Failure> {
    use std::collections::HashMap;
    let by_hwnd: HashMap<isize, &TopLevelWindow> =
        test_windows.iter().map(|w| (w.hwnd, *w)).collect();
    let intent_hwnds: std::collections::HashSet<isize> =
        intents.iter().map(|(h, _)| *h).collect();
    let mut out = Vec::new();
    let mut fail = |window: &str, problem: &str, detail: Option<(PixelRect, PixelRect)>| {
        out.push(Failure {
            iter: ctx.iter,
            target: ctx.target_name.to_string(),
            window: window.to_string(),
            problem: problem.to_string(),
            detail,
        });
    };

    // 1. Every intended placement lands where it was placed.
    for (hwnd, intent) in intents {
        let Some(w) = by_hwnd.get(hwnd) else {
            continue; // logged hwnd not among current SET windows (e.g. gone)
        };
        match intent {
            Intent::Minimize => {
                if !is_minimized(*hwnd) {
                    fail(&w.title, "should be minimized (overflow) but is not", None);
                }
            }
            Intent::Fill(cell) => {
                let Some(vis) = visible_bounds(*hwnd) else {
                    continue;
                };
                if !rect_matches(&vis, cell, 30, 60) {
                    fail(&w.title, "drifted from its parking cell", Some((vis, *cell)));
                }
            }
            Intent::Main(rect) => {
                let Some(mon) = monitor_of_center(ctx.monitors, rect.center()) else {
                    continue;
                };
                let is_main = ctx.main_ids.iter().any(|id| *id == mon.device_name);
                let Some(vis) = visible_bounds(*hwnd) else {
                    continue;
                };
                let covers = coverage(&vis, &mon.work_area_px) > 0.85;
                if !is_main {
                    fail(&w.title, "target restored onto a non-main monitor", None);
                } else if !covers && !is_maximized_like(&vis, &mon.bounds_px) {
                    fail(
                        &w.title,
                        "target on main but not (near-)full monitor",
                        Some((vis, mon.work_area_px)),
                    );
                }
            }
        }
    }

    // 2. Every live SET window is accounted for by an intent (matched+managed).
    for w in test_windows {
        if !intent_hwnds.contains(&w.hwnd) && !is_minimized(w.hwnd) {
            fail(
                &w.title,
                "live SET window not managed this switch (unmatched / left in place)",
                None,
            );
        }
    }

    // 3. No two currently-visible SET windows overlap. Each rect is eroded by a
    //    margin first: a maximized Chromium window's visible frame spills ~8px
    //    past its monitor onto the neighbour, and flush-tiled cells share an
    //    edge — neither is a real overlap. Eroding both sides absorbs those
    //    boundary slivers while any genuine overlap (a window covering another)
    //    still survives.
    const ERODE: i32 = 20;
    let visible: Vec<(&str, PixelRect)> = test_windows
        .iter()
        .filter(|w| !is_minimized(w.hwnd))
        .filter_map(|w| visible_bounds(w.hwnd).map(|r| (w.title.as_str(), erode(r, ERODE))))
        .collect();
    for i in 0..visible.len() {
        for j in (i + 1)..visible.len() {
            if overlap_area(&visible[i].1, &visible[j].1) > 4000 {
                fail(
                    &format!("{} × {}", visible[i].0, visible[j].0),
                    "two visible windows overlap",
                    Some((visible[i].1, visible[j].1)),
                );
            }
        }
    }

    out
}

/// Shrinks a rect by `m` on every side (used to drop boundary-sliver overlaps).
fn erode(r: PixelRect, m: i32) -> PixelRect {
    PixelRect::new(r.x + m, r.y + m, (r.width - 2 * m).max(0), (r.height - 2 * m).max(0))
}

fn rect_matches(a: &PixelRect, b: &PixelRect, pos_tol: i32, size_tol: i32) -> bool {
    (a.x - b.x).abs() <= pos_tol
        && (a.y - b.y).abs() <= pos_tol
        && (a.width - b.width).abs() <= size_tol
        && (a.height - b.height).abs() <= size_tol
}

fn monitor_of_center(monitors: &[MonitorInfo], center: (i32, i32)) -> Option<&MonitorInfo> {
    monitors
        .iter()
        .find(|m| m.bounds_px.contains_point(center.0, center.1))
}

fn coverage(win: &PixelRect, area: &PixelRect) -> f64 {
    let ov = overlap_area(win, area) as f64;
    let a = (area.width as f64) * (area.height as f64);
    if a <= 0.0 { 0.0 } else { ov / a }
}

fn is_maximized_like(vis: &PixelRect, bounds: &PixelRect) -> bool {
    // A maximized window's visible bounds roughly match the monitor bounds.
    (vis.width - bounds.width).abs() < 40 && (vis.height - bounds.height).abs() < 60
}

fn overlap_area(a: &PixelRect, b: &PixelRect) -> i64 {
    let x = (a.right().min(b.right()) - a.x.max(b.x)).max(0) as i64;
    let y = (a.bottom().min(b.bottom()) - a.y.max(b.y)).max(0) as i64;
    x * y
}

// ---------------------------------------------------------------------------
// Log parsing
// ---------------------------------------------------------------------------

/// Extracts `(hwnd, Intent)` pairs from a slice of freshly-appended log lines.
fn parse_intents(slice: &str) -> Vec<(isize, Intent)> {
    let mut out = Vec::new();
    for line in slice.lines() {
        if line.contains("no room, minimizing") {
            if let Some(h) = field_isize(line, "hwnd=") {
                out.push((h, Intent::Minimize));
            }
        } else if line.contains("restore target window to main") {
            if let (Some(h), Some(r)) = (field_isize(line, "hwnd="), parse_rect(line)) {
                out.push((h, Intent::Main(r)));
            }
        } else if line.contains("park window into cell")
            || line.contains("place window into cell")
            || line.contains("tiling target across main")
        {
            if let (Some(h), Some(r)) = (field_isize(line, "hwnd="), parse_rect(line)) {
                out.push((h, Intent::Fill(r)));
            }
        }
    }
    out
}

fn field_isize(line: &str, key: &str) -> Option<isize> {
    let rest = &line[line.find(key)? + key.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Parses the (single) `PixelRect { x: .., y: .., width: .., height: .. }` on a line.
fn parse_rect(line: &str) -> Option<PixelRect> {
    let start = line.find("PixelRect {")?;
    let body = &line[start..];
    let x = field_isize(body, "x: ")? as i32;
    let y = field_isize(body, "y: ")? as i32;
    let w = field_isize(body, "width: ")? as i32;
    let h = field_isize(body, "height: ")? as i32;
    Some(PixelRect::new(x, y, w, h))
}

/// Tails the newest `repodeck.YYYY-MM-DD` log file, tracking a byte offset.
struct LogTail {
    dir: PathBuf,
    path: PathBuf,
    offset: u64,
}

impl LogTail {
    fn open_newest(dir: &Path) -> Result<Self, String> {
        let path = newest_log(dir).ok_or("no log file in logs dir")?;
        Ok(Self {
            dir: dir.to_path_buf(),
            path,
            offset: 0,
        })
    }

    fn seek_to_end(&mut self) {
        self.offset = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
    }

    /// Reads bytes appended since the last read, re-resolving the newest file in
    /// case the day rolled over. Polls until `done(acc)` or `timeout`,
    /// *accumulating* every poll's bytes so a switch whose lines straddle a poll
    /// boundary is never split (the appender is non-blocking, so `begin`..`park`
    /// and `completed` can land in different reads).
    fn read_new_until(&mut self, done: impl Fn(&str) -> bool, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut acc = String::new();
        loop {
            // Handle daily rotation: if a newer file appeared, switch to it.
            if let Some(newest) = newest_log(&self.dir)
                && newest != self.path
            {
                self.path = newest;
                self.offset = 0;
            }
            acc.push_str(&self.read_from_offset());
            if done(&acc) || Instant::now() >= deadline {
                return acc;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn read_from_offset(&mut self) -> String {
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return String::new();
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            self.offset = 0; // truncated/rotated
        }
        use std::io::Seek;
        if f.seek(std::io::SeekFrom::Start(self.offset)).is_err() {
            return String::new();
        }
        let mut buf = Vec::new();
        let _ = f.read_to_end(&mut buf);
        self.offset += buf.len() as u64;
        String::from_utf8_lossy(&buf).into_owned()
    }
}

fn newest_log(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("repodeck."))
        })
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        })
}

// ---------------------------------------------------------------------------
// Pipe client + Win32 helpers
// ---------------------------------------------------------------------------

fn send_switch(id: &str) -> Result<(), String> {
    let cmd = format!("switch {id}");
    let name = HSTRING::from(TEST_CONTROL_PIPE);
    // Retry briefly: the server re-creates its single instance after each
    // message, so a back-to-back write can momentarily find no free instance.
    let mut last_err = String::new();
    for _ in 0..40 {
        // SAFETY: `name` is a valid null-terminated wide string.
        let handle = unsafe {
            CreateFileW(
                &name,
                GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        };
        match handle {
            Ok(h) if !h.is_invalid() => {
                let ok = write_all(h, cmd.as_bytes());
                // SAFETY: `h` is a valid handle we own.
                unsafe {
                    let _ = CloseHandle(h);
                }
                return ok;
            }
            _ => {
                last_err = format!("{:?}", windows::core::Error::from_thread());
                // SAFETY: `name` is valid; wait up to 200ms for an instance.
                let _ = unsafe { WaitNamedPipeW(&name, 200) };
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    Err(format!("could not reach test-control pipe: {last_err}"))
}

fn write_all(handle: HANDLE, bytes: &[u8]) -> Result<(), String> {
    let mut written = 0u32;
    // SAFETY: `handle` is a valid, writable pipe handle; `bytes`/`written` are valid.
    unsafe { WriteFile(handle, Some(bytes), Some(&mut written), None) }
        .map_err(|e| format!("WriteFile: {e}"))
}

fn is_minimized(hwnd: isize) -> bool {
    // SAFETY: raw hwnd from live enumeration; IsIconic tolerates stale handles.
    unsafe { IsIconic(windows::Win32::Foundation::HWND(hwnd as *mut _)) }.as_bool()
}

/// The window's visible on-screen bounds (DWM extended frame), matching how
/// RepoDeck itself measures a filled cell.
fn visible_bounds(hwnd: isize) -> Option<PixelRect> {
    let h = windows::Win32::Foundation::HWND(hwnd as *mut _);
    let mut r = RECT::default();
    // SAFETY: `h` may be stale; the call just fails then.
    let dwm = unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            std::ptr::from_mut(&mut r).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if dwm.is_err() {
        // SAFETY: same.
        if unsafe { GetWindowRect(h, &mut r) }.is_err() {
            return None;
        }
    }
    Some(PixelRect::new(
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
    ))
}

// ---------------------------------------------------------------------------
// Reporting + RNG
// ---------------------------------------------------------------------------

struct Failure {
    iter: usize,
    target: String,
    window: String,
    problem: String,
    detail: Option<(PixelRect, PixelRect)>,
}

struct Report {
    worksets: usize,
    switches: usize,
    noops: usize,
    /// Mismatches seen on the first measurement that resolved themselves by the
    /// confirm re-measure — i.e. windows still settling, not real drift.
    transient: usize,
    failures: Vec<Failure>,
}

impl Report {
    fn new(worksets: usize) -> Self {
        Self {
            worksets,
            switches: 0,
            noops: 0,
            transient: 0,
            failures: Vec::new(),
        }
    }

    fn print(&self) {
        println!("\n===== simtest report =====");
        println!(
            "worksets={} switches={} noops={} transient={} failures={}",
            self.worksets,
            self.switches,
            self.noops,
            self.transient,
            self.failures.len()
        );
        // Group failures by (window, problem) to keep the summary compact.
        use std::collections::BTreeMap;
        let mut groups: BTreeMap<(String, String), (usize, usize)> = BTreeMap::new();
        for f in &self.failures {
            let e = groups.entry((f.window.clone(), f.problem.clone())).or_insert((0, f.iter));
            e.0 += 1;
        }
        for ((window, problem), (count, first_iter)) in &groups {
            println!("  [{count}x] {window}: {problem} (first @iter {first_iter})");
        }
        // Show first few concrete detail lines for debugging.
        for f in self.failures.iter().take(8) {
            if let Some((a, b)) = &f.detail {
                println!(
                    "    @iter{} target={} {}: actual={:?} expected={:?}",
                    f.iter, f.target, f.window, a, b
                );
            }
        }
        if self.failures.is_empty() {
            println!("  OK — every switch placed every window where RepoDeck intended.");
        }
    }

    fn to_json(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "{{\"worksets\":{},\"switches\":{},\"noops\":{},\"transient\":{},\"failures\":[",
            self.worksets, self.switches, self.noops, self.transient
        ));
        for (i, f) in self.failures.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"iter\":{},\"target\":{:?},\"window\":{:?},\"problem\":{:?}}}",
                f.iter, f.target, f.window, f.problem
            ));
        }
        s.push_str("]}");
        s
    }
}

/// Tiny xorshift RNG — no external crate, seeded from the clock. Randomness
/// quality is irrelevant here; we only need varied switch targets.
struct Rng(u64);

impl Rng {
    fn seeded() -> Self {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self(nanos | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}
