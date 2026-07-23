//! Debug-only diagnostic — NOT part of the standard build.
//!
//! Gated behind the `debug-tools` cargo feature (see Cargo.toml's `[[bin]]`
//! `required-features`), so `cargo build --release` never produces it; build it
//! deliberately with `cargo build --release --features debug-tools` when you
//! need it.
//!
//! Reads the live state of a VS Code window (this one) once a second: window
//! geometry / show-state / monitor and the folder it has open (read from the
//! process command line, exactly like `capture_launch_spec`), plus the **git**
//! state read by RepoDeck's production git-status service and the **agent**
//! execution state persisted by the production hook integration
//! (`repodeck-hook` -> named pipe -> `runtime.json`).
//!
//! Usage: `repodeck-winstate [--title repodeck] [--folder path] [--secs 30]
//!        [--all-worksets] [--out path] [--data-dir path]`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use repodeck::application::{
    agent_status_service, gh_status_service, git_status_service, launch_service,
};
use repodeck::domain::placement::PixelRect;
use repodeck::domain::workset::LaunchKind;
use repodeck::persistence::{config_store, runtime_store};
use repodeck::windowing::enumerate::enumerate_top_level_windows;
use repodeck::windowing::monitor::enumerate_monitors;
use repodeck::windowing::{placement, process_info};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

fn main() {
    let mut title = "repodeck".to_string();
    let mut secs = 30u64;
    let mut out: Option<String> = None;
    let mut all_worksets = false;
    // Fallback folder for git when the VS Code open-folder capture returns
    // nothing (VS Code shares one process across windows, so a secondary window
    // has no folder on its command line). Defaults to the logger's cwd.
    let mut folder_fallback = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let mut data_dir = repodeck::app::local_app_data_dir().ok();
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
            "--data-dir" => {
                data_dir = args.get(i + 1).map(PathBuf::from);
                i += 1;
            }
            "--all-worksets" => {
                all_worksets = true;
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

    // One-shot "home dashboard": the same repository fields the Quick
    // Switcher shows. GitHub calls stay out of the 1s loop. `--all-worksets`
    // gives Codex the complete RepoDeck home view; without it, only the target
    // folder is projected.
    if all_worksets {
        if let Some(data_dir) = data_dir.as_deref() {
            for line in all_worksets_dashboard(data_dir) {
                let _ = writeln!(sink, "{line}");
            }
        }
    } else if let Some(folder) = folder_fallback.as_deref() {
        for line in home_dashboard(Path::new(folder), data_dir.as_deref()) {
            let _ = writeln!(sink, "{line}");
        }
        let _ = sink.flush();
    }

    eprintln!("[winstate] watching VS Code window title~=\"{title}\" for {secs}s, 1 line/sec");
    let start = Instant::now();
    let mut tick = 0u64;
    while tick < secs {
        let line = capture_line(
            &title,
            folder_fallback.as_deref(),
            data_dir.as_deref(),
            tick,
        );
        let _ = writeln!(sink, "{line}");
        let _ = sink.flush();
        if out.is_some() {
            eprintln!("{line}");
        }
        tick += 1;
        if tick < secs {
            // Keep a fixed one-second cadence. Sleeping a full second after
            // capture would make each period "capture time + 1 second" and
            // silently lose samples whenever git/window inspection is slow.
            let next_tick = start + Duration::from_secs(tick);
            std::thread::sleep(next_tick.saturating_duration_since(Instant::now()));
        }
    }
    eprintln!("[winstate] done ({tick} samples)");
}

/// Builds one state line for the target VS Code window, re-reading everything
/// live each tick (the window may move, resize, minimize, or change folder).
fn capture_line(
    title_needle: &str,
    folder_fallback: Option<&str>,
    data_dir: Option<&Path>,
    tick: u64,
) -> String {
    let stamp = format!("t={tick:>4}s");

    let windows = enumerate_top_level_windows(std::process::id());
    // "This window": a VS Code window whose title contains the needle.
    let window = windows.as_ref().ok().and_then(|windows| {
        windows.iter().find(|window| {
            window
                .executable_path
                .as_deref()
                .map(launch_service::classify)
                .map(|kind| kind == LaunchKind::VsCode)
                .unwrap_or(false)
                && window.title.contains(title_needle)
        })
    });

    // The folder the window has open — the non-trivial "VS Code state", read
    // the same way `capture_launch_spec` does (process command line → folder).
    let folder = window.and_then(|window| {
        process_info::read_process_command_line(window.process_id)
            .and_then(|command_line| launch_service::extract_vscode_folder(&command_line))
    });

    // Git and agent state use the same production services/stores as RepoDeck.
    // Falling back to `--folder` is necessary for a secondary VS Code window:
    // VS Code commonly shares a process whose command line has no folder.
    let observed_folder = folder.as_deref().or(folder_fallback);
    let git = observed_folder
        .map(|path| git_state(Path::new(path)))
        .unwrap_or_else(|| "git=?".into());
    let agent = match (observed_folder, data_dir) {
        (Some(path), Some(data_dir)) => agent_state(data_dir, Path::new(path)),
        _ => "agent=?".into(),
    };

    let window_projection = match window {
        Some(window) => {
            // Geometry + show state, straight from RepoDeck's window layer.
            let hwnd = HWND(window.hwnd as *mut _);
            let frame = window_rect(hwnd);
            let show = placement::get_show_state(hwnd)
                .map(|state| format!("{state:?}"))
                .unwrap_or_else(|_| "?".into());
            let monitor = frame
                .as_ref()
                .and_then(|rect| {
                    let (cx, cy) = rect.center();
                    enumerate_monitors()
                        .ok()?
                        .into_iter()
                        .find(|monitor| monitor.bounds_px.contains_point(cx, cy))
                        .map(|monitor| monitor.device_name)
                })
                .unwrap_or_else(|| "?".into());
            let geometry = frame
                .map(|rect| {
                    format!(
                        "frame=({},{} {}x{})",
                        rect.x, rect.y, rect.width, rect.height
                    )
                })
                .unwrap_or_else(|| "frame=?".into());
            format!(
                "window=[show={show} monitor={monitor} {geometry} pid={} hwnd={}]",
                window.process_id, window.hwnd
            )
        }
        None => match windows {
            Ok(_) => format!("window=[not-found title~={title_needle:?}]"),
            Err(error) => format!("window=[enumerate-error:{error}]"),
        },
    };

    format!(
        "{stamp} {agent} {git} {window_projection} folder={:?}",
        observed_folder.unwrap_or("(none)")
    )
}

/// One-shot per-repository summary — every field the home-screen dashboard
/// wants, gathered from the production services (git + gh + agent hooks). Slow
/// network fields (PRs / CI) are fetched here once, not per tick.
fn home_dashboard(folder: &Path, data_dir: Option<&Path>) -> Vec<String> {
    let git = git_status_service::fetch(folder);
    let branch = git.branch.clone().unwrap_or_else(|| "-".into());
    let gh = gh_status_service::fetch(folder, git.branch.as_deref());
    let (ci_sym, ci_label) = gh.ci.symbol_label();
    let pr = gh
        .current_branch_pr
        .as_ref()
        .map(|p| format!("PR #{} {:?}", p.number, p.title))
        .unwrap_or_else(|| "-".into());
    let agent = data_dir
        .map(|d| agent_state(d, folder))
        .unwrap_or_else(|| "agent=?".into());

    vec![
        "===== home dashboard (one-shot) =====".into(),
        format!("  repo            : {}", folder.display()),
        format!("  branch          : {branch}"),
        format!("  uncommitted     : {} 変更", git.changed_count),
        format!(
            "  unpushed        : {}",
            if git.has_upstream {
                format!("{} commits", git.ahead)
            } else {
                "(no upstream)".into()
            }
        ),
        format!(
            "  last commit     : {}",
            git.last_commit_at.as_deref().unwrap_or("-")
        ),
        format!("  open PRs        : {}", gh.open_pr_count),
        format!("  this branch PR  : {pr}"),
        format!("  CI              : {ci_sym} {ci_label}"),
        format!("  agent (担当)    : {agent}"),
        format!("  gh available    : {}", gh.gh_available),
        "=====================================".into(),
    ]
}

/// One parse-friendly line per configured workset for Codex's RepoDeck home
/// view. Repository probes run in parallel because each `gh` call has its own
/// timeout and serial execution would multiply that timeout by the workset
/// count.
fn all_worksets_dashboard(data_dir: &Path) -> Vec<String> {
    let config = match config_store::load(data_dir) {
        Ok(Some(loaded)) => loaded.config,
        Ok(None) => return vec!["repositories=[no-config]".into()],
        Err(error) => return vec![format!("repositories=[config-error:{error}]")],
    };
    let worksets: Vec<_> = config
        .worksets
        .into_iter()
        .filter(|workset| !workset.repository_path.as_os_str().is_empty())
        .collect();

    let rows = std::thread::scope(|scope| {
        let handles: Vec<_> = worksets
            .into_iter()
            .map(|workset| {
                scope.spawn(move || {
                    let folder = repodeck::application::workset_service::resolve_match_path(
                        &workset.repository_path,
                        workset.repository_kind,
                    );
                    repository_dashboard_line(&workset.name, &folder, workset.sort_order, data_dir)
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect::<Vec<_>>()
    });

    let mut lines = vec!["===== repositories (Quick Switcher projection) =====".into()];
    let mut rows = rows;
    rows.sort_by_key(|(sort_order, _)| *sort_order);
    lines.extend(rows.into_iter().map(|(_, line)| line));
    lines.push("====================================================".into());
    lines
}

fn repository_dashboard_line(
    name: &str,
    folder: &Path,
    sort_order: i32,
    data_dir: &Path,
) -> (i32, String) {
    let git = git_status_service::fetch(folder);
    let gh = gh_status_service::fetch(folder, git.branch.as_deref());
    let (ci_symbol, ci_label) = gh.ci.symbol_label();
    let ci = if gh.gh_available {
        format!("{ci_symbol} {ci_label}")
    } else {
        "unavailable".into()
    };
    let pr = gh
        .current_branch_pr
        .as_ref()
        .map(|pr| format!("#{}", pr.number))
        .unwrap_or_else(|| "-".into());
    let unpushed = if git.has_upstream {
        git.ahead.to_string()
    } else {
        "no-upstream".into()
    };
    let (owners, last_agent_activity) = agent_owners_and_activity(data_dir, folder);

    (
        sort_order,
        format!(
            "repo={name:?} path={:?} branch={:?} changed={} unpushed={} open_prs={} pr={pr:?} ci={ci:?} gh_available={} owners={owners:?} last_commit={:?} last_agent_activity={last_agent_activity:?}",
            folder.display().to_string(),
            git.branch.as_deref().unwrap_or("-"),
            git.changed_count,
            unpushed,
            gh.open_pr_count,
            gh.gh_available,
            git.last_commit_at.as_deref().unwrap_or("-"),
        ),
    )
}

/// Git projection produced by the exact service used to fill Quick Switcher.
fn git_state(folder: &Path) -> String {
    let git = git_status_service::fetch(folder);
    if !git.is_git {
        return "git=[not-repository]".into();
    }
    let unpushed = if git.has_upstream {
        format!("{}", git.ahead)
    } else {
        "no-upstream".into()
    };
    format!(
        "git=[branch={} changed={} unpushed={} behind={} last_commit={}]",
        git.branch.as_deref().unwrap_or("-"),
        git.changed_count,
        unpushed,
        git.behind,
        git.last_commit_at.as_deref().unwrap_or("-")
    )
}

/// Agent projection produced from the persisted result of RepoDeck's actual
/// hook path. No transcript/process heuristic is involved.
fn agent_state(data_dir: &Path, folder: &Path) -> String {
    let config = match config_store::load(data_dir) {
        Ok(Some(loaded)) => loaded.config,
        Ok(None) => return "agent=[no-config]".into(),
        Err(error) => return format!("agent=[config-error:{error}]"),
    };

    let Some(workset_id) = agent_status_service::map_event_to_workset(&config.worksets, folder)
    else {
        return "agent=[unmatched-workset]".into();
    };

    let runtime = runtime_store::load(data_dir);
    let aggregate = agent_status_service::aggregate_all(&config.worksets, &runtime.agent_runs)
        .get(&workset_id)
        .copied()
        .unwrap_or(repodeck::domain::agent::AgentState::Unknown);
    let workset_name = config
        .worksets
        .iter()
        .find(|workset| workset.id == workset_id)
        .map(|workset| workset.name.as_str())
        .unwrap_or("?");
    let runs = runtime
        .agent_runs
        .iter()
        .filter(|run| run.workset_id == workset_id)
        .map(|run| {
            let owner = if run.turn_id.is_empty() {
                "Claude"
            } else {
                "Codex"
            };
            format!(
                "{owner}/{}:{}={:?}",
                run.session_id,
                if run.turn_id.is_empty() {
                    "-"
                } else {
                    &run.turn_id
                },
                run.state
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    format!("agent=[workset={workset_name:?} aggregate={aggregate:?} runs={runs:?}]")
}

fn agent_owners_and_activity(data_dir: &Path, folder: &Path) -> (String, String) {
    let Ok(Some(loaded)) = config_store::load(data_dir) else {
        return ("-".into(), "-".into());
    };
    let Some(workset_id) =
        agent_status_service::map_event_to_workset(&loaded.config.worksets, folder)
    else {
        return ("-".into(), "-".into());
    };
    let runtime = runtime_store::load(data_dir);
    let mut owners: Vec<&str> = runtime
        .agent_runs
        .iter()
        .filter(|run| run.workset_id == workset_id)
        .map(|run| {
            if run.turn_id.is_empty() {
                "Claude"
            } else {
                "Codex"
            }
        })
        .collect();
    owners.sort_unstable();
    owners.dedup();
    let owners = if owners.is_empty() {
        "-".into()
    } else {
        owners.join(",")
    };
    let last_activity = runtime
        .agent_runs
        .iter()
        .filter(|run| run.workset_id == workset_id)
        .map(|run| run.last_transition_at.as_str())
        .max()
        .unwrap_or("-");
    (owners, last_activity.to_string())
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
