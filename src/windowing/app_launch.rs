//! Spawns a [`LaunchSpec`]'s process to reopen a closed managed-window app
//! (PLAN.md §5.3 extension). The decision of *what* to launch is the pure
//! [`crate::application::launch_service`]; this module only performs the spawn.

use std::process::Command;

use crate::domain::workset::LaunchSpec;

/// Launches `spec`'s program with its args, fully detached from RepoDeck. The
/// spawned child is intentionally not waited on — it's a normal desktop app
/// with its own lifetime, and RepoDeck re-adopts its window by matching, not
/// by owning the process.
pub fn launch(spec: &LaunchSpec) -> std::io::Result<()> {
    // A packaged (MSIX/Store) app is activated by its AUMID via the shell's
    // AppsFolder, never by its install path — that path carries the package
    // version and so changes on every update. Fall back to the recorded exe if
    // the shell activation fails (wrong app id, package removed, …).
    if let Some(aumid) = spec.aumid.as_deref()
        && launch_store_app(aumid).is_ok()
    {
        return Ok(());
    }

    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    // Detach: don't inherit RepoDeck's stdio, and let the child outlive us.
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for name in INHERITED_VARS_TO_DROP {
        command.env_remove(name);
    }
    // Spawn and immediately drop the handle; the child keeps running.
    let _child = command.spawn()?;
    Ok(())
}

/// 子プロセスへ引き継いではいけない環境変数。
///
/// RepoDeck を VS Code のターミナルから起動すると、拡張ホストの環境が丸ごと
/// 引き継がれる。その中の `ELECTRON_RUN_AS_NODE=1` は Electron アプリを素の
/// Node として起動させるので、`Code.exe --new-window <folder>` が
/// `bad option: --new-window` で即死し、開き直しても VS Code が永久に現れない。
/// `VSCODE_*` の受け渡し変数も同様に、別インスタンスの IPC や NLS 設定を
/// 押し付けてしまう。起動するのは独立したデスクトップアプリなので、これらは
/// 落としてから渡す。
const INHERITED_VARS_TO_DROP: &[&str] = &[
    "ELECTRON_RUN_AS_NODE",
    "VSCODE_CODE_CACHE_PATH",
    "VSCODE_CRASH_REPORTER_PROCESS_TYPE",
    "VSCODE_CWD",
    "VSCODE_ESM_ENTRYPOINT",
    "VSCODE_HANDLES_UNCAUGHT_ERRORS",
    "VSCODE_IPC_HOOK",
    "VSCODE_L10N_BUNDLE_LOCATION",
    "VSCODE_NLS_CONFIG",
    "VSCODE_PID",
];

/// Activates a packaged app by AUMID through `explorer.exe shell:AppsFolder\…`,
/// the documented shell entry point for launching a Store app without knowing
/// where it is installed. Explorer returns immediately; success here only means
/// the request was handed off.
fn launch_store_app(aumid: &str) -> std::io::Result<()> {
    let mut command = Command::new("explorer.exe");
    command
        .arg(format!(r"shell:AppsFolder\{aumid}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _child = command.spawn()?;
    Ok(())
}
