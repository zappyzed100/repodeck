//! Debug-only diagnostic — NOT part of the standard build.
//!
//! Gated behind the `debug-tools` cargo feature (see Cargo.toml's `[[bin]]`
//! `required-features`), so `cargo build --release` never produces it; build it
//! deliberately with `cargo build --release --features debug-tools` when you
//! need it.
//!
//! Reads the live state of a VS Code window (this one) once a second: window
//! geometry / show-state / monitor and the folder it has open (read from the
//! process command line, exactly like `capture_launch_spec`), plus its **git**
//! state (branch + dirty count) and whether **Claude Code** is running or
//! waiting for input — inferred from how recently this session's transcript
//! (`~/.claude/projects/<slug>/<id>.jsonl`) was appended to. For the accurate,
//! production path to Claude Code's state, use the hook integration
//! (`ipc::protocol` + `repodeck-hook`) instead.
//!
//! Usage: `repodeck-winstate [--title repodeck] [--secs 30] [--out path]`.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use repodeck::application::launch_service;
use repodeck::domain::placement::PixelRect;
use repodeck::domain::workset::LaunchKind;
use repodeck::windowing::enumerate::enumerate_top_level_windows;
use repodeck::windowing::monitor::enumerate_monitors;
use repodeck::windowing::{placement, process_info};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

fn main() {
    let mut title = "repodeck".to_string();
    let mut secs = 30u64;
    let mut out: Option<String> = None;
    // Fallback folder for git when the VS Code open-folder capture returns
    // nothing (VS Code shares one process across windows, so a secondary window
    // has no folder on its command line). Defaults to the logger's cwd.
    let mut folder_fallback = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--title" => {
                if let Some(v) = args.get(i + 1) {
                    title = v.clone();
                }
                i += 1;
            }
            "--folder" => {
                folder_fallback = args.get(i + 1).cloned();
                i += 1;
            }
            "--secs" => {
                secs = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(secs);
                i += 1;
            }
            "--out" => {
                out = args.get(i + 1).cloned();
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    let mut sink: Box<dyn Write> = match &out {
        Some(p) => Box::new(std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .expect("open log file"),
        )),
        None => Box::new(std::io::stdout()),
    };

    eprintln!("[winstate] watching VS Code window title~=\"{title}\" for {secs}s, 1 line/sec");
    let start = Instant::now();
    let mut tick = 0u64;
    while start.elapsed().as_secs() < secs {
        let line = capture_line(&title, folder_fallback.as_deref(), tick);
        let _ = writeln!(sink, "{line}");
        let _ = sink.flush();
        if out.is_some() {
            eprintln!("{line}");
        }
        tick += 1;
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!("[winstate] done ({tick} samples)");
}

/// Builds one state line for the target VS Code window, re-reading everything
/// live each tick (the window may move, resize, minimize, or change folder).
fn capture_line(title_needle: &str, folder_fallback: Option<&str>, tick: u64) -> String {
    let stamp = format!("t={tick:>4}s");

    let windows = match enumerate_top_level_windows(std::process::id()) {
        Ok(w) => w,
        Err(e) => return format!("{stamp} ERROR enumerate: {e}"),
    };

    // "This window": a VS Code window whose title contains the needle.
    let Some(w) = windows.iter().find(|w| {
        w.executable_path
            .as_deref()
            .map(launch_service::classify)
            .map(|k| k == LaunchKind::VsCode)
            .unwrap_or(false)
            && w.title.contains(title_needle)
    }) else {
        return format!("{stamp} (no VS Code window matching \"{title_needle}\" — minimized/closed?)");
    };

    // Geometry + show state, straight from RepoDeck's window layer.
    let hwnd = HWND(w.hwnd as *mut _);
    let frame = window_rect(hwnd);
    let show = placement::get_show_state(hwnd)
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|_| "?".into());

    // Which monitor the window sits on (by its centre).
    let monitor = frame
        .as_ref()
        .and_then(|r| {
            let (cx, cy) = r.center();
            enumerate_monitors()
                .ok()?
                .into_iter()
                .find(|m| m.bounds_px.contains_point(cx, cy))
                .map(|m| m.device_name)
        })
        .unwrap_or_else(|| "?".into());

    // The folder the window has open — the non-trivial "VS Code state", read the
    // same way `capture_launch_spec` does (process command line → folder).
    let folder = process_info::read_process_command_line(w.process_id)
        .and_then(|cl| launch_service::extract_vscode_folder(&cl));

    let geo = frame
        .map(|r| format!("frame=({},{} {}x{})", r.x, r.y, r.width, r.height))
        .unwrap_or_else(|| "frame=?".into());

    // git state of the open folder (branch + dirty count), falling back to the
    // provided folder when VS Code exposed no path on its command line.
    let git = folder
        .as_deref()
        .or(folder_fallback)
        .map(git_state)
        .unwrap_or_else(|| "git=?".into());

    // Claude Code state for this session (running / waiting-for-input).
    let claude = claude_status();

    format!(
        "{stamp} claude={claude} {git} show={show} monitor={monitor} {geo} folder={:?} pid={} hwnd={}",
        folder.as_deref().unwrap_or("(none)"),
        w.process_id,
        w.hwnd
    )
}

/// `branch=<name> dirty=<n>` for the repo at `folder`, via the git CLI (best
/// effort — the same information RepoDeck shows in the Quick Switcher row).
fn git_state(folder: &str) -> String {
    let run = |args: &[&str]| -> Option<String> {
        let o = std::process::Command::new("git")
            .args(["-C", folder])
            .args(args)
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "?".into());
    let dirty = run(&["status", "--porcelain"])
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);
    format!("git=[branch={branch} dirty={dirty}]")
}

/// RUNNING vs WAITING-INPUT for the active Claude Code session, inferred from
/// how recently its transcript JSONL was appended to: while Claude Code works it
/// streams tool calls / output into the transcript, so a fresh mtime means it is
/// executing; once it hands the turn back to the user the file goes quiet.
fn claude_status() -> String {
    let Some(base) = std::env::var_os("USERPROFILE") else {
        return "CLAUDE=?".into();
    };
    let dir = Path::new(&base)
        .join(".claude")
        .join("projects")
        .join("c--code-portfolio-repodeck");
    // Newest .jsonl in the project dir is the active session's transcript.
    let newest = std::fs::read_dir(&dir).ok().and_then(|rd| {
        rd.filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|p| p.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (p, t)))
            .max_by_key(|(_, t)| *t)
    });
    let Some((_, modified)) = newest else {
        return "CLAUDE=(no session)".into();
    };
    let age = modified.elapsed().map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let state = if age < 3.0 {
        "RUNNING"
    } else if age < 90.0 {
        "WAITING-INPUT"
    } else {
        "IDLE"
    };
    format!("{state}(idle={age:.0}s)")
}

fn window_rect(hwnd: HWND) -> Option<PixelRect> {
    let mut r = RECT::default();
    // SAFETY: `hwnd` came from a live enumeration this tick; GetWindowRect just
    // fails harmlessly if it went stale.
    unsafe { GetWindowRect(hwnd, &mut r) }.ok()?;
    Some(PixelRect::new(
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
    ))
}
