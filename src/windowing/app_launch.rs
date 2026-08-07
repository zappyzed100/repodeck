//! Spawns a [`LaunchSpec`]'s process to reopen a closed managed-window app
//! (PLAN.md §5.3 extension). The decision of *what* to launch is the pure
//! [`crate::application::launch_service`]; this module only performs the spawn.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

use crate::domain::workset::{LaunchKind, LaunchSpec};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary,
};
use windows::Win32::System::Threading::{
    CREATE_PROCESS_LOGON_FLAGS, CREATE_UNICODE_ENVIRONMENT, CreateProcessWithTokenW, OpenProcess,
    OpenProcessToken, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
use windows::core::{PCWSTR, PWSTR};

/// Launches `spec`'s program with its args, fully detached from RepoDeck. The
/// spawned child is intentionally not waited on — it's a normal desktop app
/// with its own lifetime, and RepoDeck re-adopts its window by matching, not
/// by owning the process.
pub fn launch(spec: &LaunchSpec) -> std::io::Result<()> {
    let launch_vscode_unelevated =
        spec.kind == LaunchKind::VsCode && crate::windowing::elevation::is_elevated();

    // A packaged (MSIX/Store) app is activated by its AUMID via the shell's
    // AppsFolder, never by its install path — that path carries the package
    // version and so changes on every update. Fall back to the recorded exe if
    // the shell activation fails (wrong app id, package removed, …).
    if let Some(aumid) = spec.aumid.as_deref()
        && (if launch_vscode_unelevated {
            launch_store_app_unelevated(aumid)
        } else {
            launch_store_app(aumid)
        })
        .is_ok()
    {
        return Ok(());
    }

    // RepoDeck deliberately runs elevated so it can manage elevated windows.
    // A direct child would inherit that high-integrity token, however, which
    // makes VS Code itself run as administrator. Start VS Code with the
    // interactive desktop shell's medium-integrity token instead.
    if launch_vscode_unelevated {
        return launch_unelevated(&spec.program, &spec.args);
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

/// Launches an executable with the token of the interactive Explorer process.
///
/// `Command::spawn` inherits RepoDeck's elevated token, so it cannot be used
/// for VS Code while RepoDeck is elevated. The shell process belongs to the
/// signed-in desktop user and provides the normal (medium-integrity) token
/// needed to launch a desktop app without UAC elevation.
fn launch_unelevated(program: &Path, args: &[String]) -> io::Result<()> {
    let token = interactive_shell_token()?;
    let program_wide = to_wide(program.as_os_str());
    let mut command_line = command_line(program.as_os_str(), args);
    let environment = environment_block_without_inherited_vars();
    let startup = STARTUPINFOW {
        cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>()).unwrap_or(0),
        ..Default::default()
    };
    let mut process_info = PROCESS_INFORMATION::default();

    // `CreateProcessWithTokenW` may modify the command-line buffer, so it must
    // be mutable and remain alive until the call returns. The application name
    // is supplied separately to avoid any ambiguity around a quoted path.
    let result = unsafe {
        CreateProcessWithTokenW(
            token,
            CREATE_PROCESS_LOGON_FLAGS(0),
            PCWSTR(program_wide.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            CREATE_UNICODE_ENVIRONMENT,
            Some(environment.as_ptr().cast()),
            PCWSTR::null(),
            &startup,
            &mut process_info,
        )
    };

    // The token and the returned process/thread handles are all owned by this
    // call. The child itself continues running after these handles close.
    unsafe {
        let _ = CloseHandle(token);
        if !process_info.hProcess.is_invalid() {
            let _ = CloseHandle(process_info.hProcess);
        }
        if !process_info.hThread.is_invalid() {
            let _ = CloseHandle(process_info.hThread);
        }
    }

    result.map_err(|err| io::Error::other(format!("CreateProcessWithTokenW failed: {err}")))
}

fn to_wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Gets the primary token of the Explorer process that owns the interactive
/// desktop. The caller must close the returned token handle.
fn interactive_shell_token() -> io::Result<HANDLE> {
    let shell_window = unsafe { GetShellWindow() };
    if shell_window.is_invalid() {
        return Err(io::Error::other(
            "the interactive Explorer window was not found",
        ));
    }

    let mut process_id = 0u32;
    if unsafe { GetWindowThreadProcessId(shell_window, Some(&mut process_id)) } == 0
        || process_id == 0
    {
        return Err(io::Error::other(
            "the interactive Explorer process was not found",
        ));
    }

    let process = unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id)
            .map_err(|err| io::Error::other(format!("OpenProcess failed: {err}")))?
    };
    let mut source_token = HANDLE::default();
    let open_token = unsafe {
        OpenProcessToken(process, TOKEN_DUPLICATE | TOKEN_QUERY, &mut source_token)
            .map_err(|err| io::Error::other(format!("OpenProcessToken failed: {err}")))
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    open_token?;

    // CreateProcessWithTokenW is more reliable with a duplicated primary
    // token than with the handle returned directly from Explorer, especially
    // when the caller is an elevated process with a filtered admin token.
    let mut primary_token = HANDLE::default();
    let duplicate = unsafe {
        DuplicateTokenEx(
            source_token,
            TOKEN_ASSIGN_PRIMARY
                | TOKEN_DUPLICATE
                | TOKEN_QUERY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary_token,
        )
        .map_err(|err| io::Error::other(format!("DuplicateTokenEx failed: {err}")))
    };
    unsafe {
        let _ = CloseHandle(source_token);
    }
    duplicate.map(|()| primary_token)
}

/// Builds a mutable Windows command line from the executable and its args.
fn command_line(program: &OsStr, args: &[String]) -> Vec<u16> {
    let mut result = Vec::new();
    append_quoted_arg(&mut result, program);
    for arg in args {
        result.push(' ' as u16);
        append_quoted_arg(&mut result, OsStr::new(arg));
    }
    result.push(0);
    result
}

/// Applies the CommandLineToArgvW quoting rules to one UTF-16 argument.
fn append_quoted_arg(result: &mut Vec<u16>, arg: &OsStr) {
    let units: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|unit| *unit == b' ' as u16 || *unit == b'\t' as u16 || *unit == b'"' as u16);
    if !needs_quotes {
        result.extend(units);
        return;
    }

    result.push('"' as u16);
    let mut backslashes = 0usize;
    for unit in units {
        if unit == '\\' as u16 {
            backslashes += 1;
        } else if unit == '"' as u16 {
            result.extend(std::iter::repeat_n('\\' as u16, backslashes * 2 + 1));
            result.push(unit);
            backslashes = 0;
        } else {
            result.extend(std::iter::repeat_n('\\' as u16, backslashes));
            result.push(unit);
            backslashes = 0;
        }
    }
    result.extend(std::iter::repeat_n('\\' as u16, backslashes * 2));
    result.push('"' as u16);
}

/// Makes an environment block while preserving the existing launch hygiene.
fn environment_block_without_inherited_vars() -> Vec<u16> {
    let mut result = Vec::new();
    for (name, value) in std::env::vars_os() {
        if INHERITED_VARS_TO_DROP
            .iter()
            .any(|drop| name.to_string_lossy().eq_ignore_ascii_case(drop))
        {
            continue;
        }
        result.extend(name.encode_wide());
        result.push('=' as u16);
        result.extend(value.encode_wide());
        result.push(0);
    }
    result.push(0);
    result
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

fn launch_store_app_unelevated(aumid: &str) -> std::io::Result<()> {
    launch_unelevated(
        Path::new("explorer.exe"),
        &[format!(r"shell:AppsFolder\{aumid}")],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_paths_and_preserves_arguments() {
        let command_line = command_line(
            OsStr::new(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            &["-n".to_string(), r"C:\workspaces\my repo".to_string()],
        );
        let actual = String::from_utf16(&command_line)
            .expect("the generated command line must be valid UTF-16")
            .trim_end_matches('\0')
            .to_string();
        assert_eq!(
            actual,
            r#""C:\Program Files\Microsoft VS Code\Code.exe" -n "C:\workspaces\my repo""#
        );
    }

    #[test]
    fn command_line_escapes_trailing_backslashes_inside_quotes() {
        let command_line = command_line(OsStr::new("code.exe"), &[r"C:\work repo\".to_string()]);
        let actual = String::from_utf16(&command_line)
            .expect("the generated command line must be valid UTF-16")
            .trim_end_matches('\0')
            .to_string();
        assert_eq!(actual, r#"code.exe "C:\work repo\\""#);
    }
}
