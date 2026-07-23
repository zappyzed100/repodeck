//! Enumerates installed Store / MSIX apps, so a packaged app can be registered
//! as a launch candidate.
//!
//! A packaged app has **no `.lnk` in the Start Menu** — Windows lists it from
//! the shell's Apps folder instead — so `start_menu` alone can never see it
//! (ChatGPT is the case that surfaced this: `Get-StartApps` shows it as
//! `OpenAI.Codex_2p2nqsd0c76g0!App`, but no shortcut file exists). Nor can the
//! user browse to its executable: packaged apps install under
//! `C:\Program Files\WindowsApps`, whose ACL blocks the file picker.
//!
//! So the route has to come from the shell:
//!
//! 1. enumerate the Apps folder — each item's *parsing name* is its AUMID;
//! 2. split the AUMID into package family name and application id;
//! 3. resolve the family to its install directory (`GetPackagesByPackageFamily`
//!    → `GetPackagePathByFullName`);
//! 4. read `AppxManifest.xml` for that application id's `Executable`.
//!
//! Step 4 matters: RepoDeck matches a window to its workset partly by
//! executable path, and that scores 50 of the 75 points auto-rebind needs — an
//! entry with only an AUMID would relaunch the app on every switch instead of
//! recognising the window it already opened. The install path carries the
//! package *version*, so it changes on update; that is fine for launching
//! (`launch_service::store_app_aumid` re-derives the version-independent AUMID
//! from it) and only costs a re-registration for matching after an update.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows::Win32::Storage::Packaging::Appx::{
    GetPackagePathByFullName, GetPackagesByPackageFamily,
};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, KF_FLAG_DEFAULT,
    SHGetKnownFolderItem, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// One installed packaged app that resolves to a runnable executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreApp {
    /// The name Windows shows in the Start Menu, e.g. "ChatGPT".
    pub name: String,
    /// Application User Model ID, e.g. `OpenAI.Codex_2p2nqsd0c76g0!App`.
    pub aumid: String,
    /// The executable inside the package's install directory.
    pub executable: PathBuf,
}

/// Every packaged app whose executable could be resolved, sorted by name.
///
/// Best-effort throughout: an app whose package or manifest can't be read is
/// skipped rather than failing the whole enumeration. Unpackaged entries in the
/// Apps folder (ordinary `.exe`s and `.lnk`s) are ignored — `start_menu` covers
/// those, and its shortcut names are the better label.
pub fn enumerate() -> Vec<StoreApp> {
    let mut apps: Vec<StoreApp> = apps_folder_entries()
        .into_iter()
        .filter_map(|(name, aumid)| {
            let executable = resolve_executable(&aumid)?;
            Some(StoreApp {
                name,
                aumid,
                executable,
            })
        })
        .collect();

    apps.sort_by_key(|app| app.name.to_lowercase());
    apps.dedup_by(|a, b| a.aumid == b.aumid);
    apps
}

/// `(display name, AUMID)` for every packaged app in the shell's Apps folder.
///
/// The parsing name of a packaged item *is* its AUMID (`<family>!<app id>`);
/// unpackaged items parse to a file path instead, which is how they are told
/// apart — an AUMID always contains `!` and never a path separator.
fn apps_folder_entries() -> Vec<(String, String)> {
    let mut entries = Vec::new();

    // SAFETY: the Apps folder is a documented known folder; every interface
    // obtained here is released when its wrapper drops at the end of scope.
    unsafe {
        let Ok(folder) =
            SHGetKnownFolderItem::<IShellItem>(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)
        else {
            return entries;
        };
        let Ok(items) =
            folder.BindToHandler::<Option<&IBindCtx>, IEnumShellItems>(None, &BHID_EnumItems)
        else {
            return entries;
        };

        loop {
            let mut fetched = [const { None }; 1];
            let mut count = 0u32;
            if items.Next(&mut fetched, Some(&mut count)).is_err() || count == 0 {
                break;
            }
            let Some(item) = fetched[0].take() else {
                break;
            };

            let Some(parsing_name) = display_name(&item, SIGDN_PARENTRELATIVEPARSING) else {
                continue;
            };
            if !parsing_name.contains('!')
                || parsing_name.contains('\\')
                || parsing_name.contains('/')
            {
                continue;
            }
            let Some(name) = display_name(&item, SIGDN_NORMALDISPLAY) else {
                continue;
            };
            entries.push((name, parsing_name));
        }
    }

    entries
}

/// Reads one of the item's display names, freeing the shell's buffer.
///
/// # Safety
/// Must be called on a thread with COM initialised (the caller enumerates the
/// shell folder there).
unsafe fn display_name(
    item: &IShellItem,
    kind: windows::Win32::UI::Shell::SIGDN,
) -> Option<String> {
    // SAFETY: `item` is a live interface; `GetDisplayName` returns a
    // CoTaskMem-allocated string which is freed here after being copied.
    unsafe {
        let raw = item.GetDisplayName(kind).ok()?;
        let text = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0.cast()));
        text.filter(|s| !s.is_empty())
    }
}

/// The executable an AUMID launches, or `None` if the package or its manifest
/// can't be read.
fn resolve_executable(aumid: &str) -> Option<PathBuf> {
    let (family, application_id) = aumid.split_once('!')?;
    let install_dir = package_install_dir(family)?;
    let relative = manifest_executable(&install_dir, application_id)?;
    // Manifest paths use forward slashes ("app/ChatGPT.exe").
    let mut path = install_dir;
    for part in relative.split(['/', '\\']).filter(|p| !p.is_empty()) {
        path.push(part);
    }
    path.exists().then_some(path)
}

/// The install directory of any package in `family` (they differ only by
/// version/architecture, and the newest registered one is what launches).
fn package_install_dir(family: &str) -> Option<PathBuf> {
    let full_name = package_full_name(family)?;
    let wide = HSTRING::from(full_name);

    let mut length = 0u32;
    // SAFETY: the first call only measures — a null buffer with length 0 is the
    // documented way to ask for the required size.
    let status = unsafe { GetPackagePathByFullName(PCWSTR(wide.as_ptr()), &mut length, None) };
    if status != ERROR_INSUFFICIENT_BUFFER || length == 0 {
        return None;
    }

    let mut buffer = vec![0u16; length as usize];
    // SAFETY: `buffer` holds `length` wide characters, exactly what the
    // measuring call above asked for.
    let status = unsafe {
        GetPackagePathByFullName(
            PCWSTR(wide.as_ptr()),
            &mut length,
            Some(PWSTR(buffer.as_mut_ptr())),
        )
    };
    if status.is_err() {
        return None;
    }
    Some(PathBuf::from(wide_to_string(&buffer)))
}

/// The first package full name registered for `family`.
fn package_full_name(family: &str) -> Option<String> {
    let wide = HSTRING::from(family);

    let mut count = 0u32;
    let mut buffer_length = 0u32;
    // SAFETY: measuring call — null out-pointers with zero counts, as documented.
    let status = unsafe {
        GetPackagesByPackageFamily(
            PCWSTR(wide.as_ptr()),
            &mut count,
            None,
            &mut buffer_length,
            None,
        )
    };
    if status != ERROR_INSUFFICIENT_BUFFER || count == 0 {
        return None;
    }

    let mut names = vec![PWSTR::null(); count as usize];
    let mut buffer = vec![0u16; buffer_length as usize];
    // SAFETY: both buffers are sized by the measuring call; `names` receives
    // pointers into `buffer`, which outlives the read below.
    let status = unsafe {
        GetPackagesByPackageFamily(
            PCWSTR(wide.as_ptr()),
            &mut count,
            Some(names.as_mut_ptr()),
            &mut buffer_length,
            Some(PWSTR(buffer.as_mut_ptr())),
        )
    };
    if status.is_err() {
        return None;
    }
    // SAFETY: on success each entry points into `buffer` and is null-terminated.
    unsafe { names.first().and_then(|n| n.to_string().ok()) }
}

/// The `Executable` attribute of the `<Application>` whose `Id` is
/// `application_id`, read from the package's `AppxManifest.xml`.
///
/// Scanned as text rather than parsed as XML: the manifest is machine-generated
/// and this needs two attributes of one element, so a real XML dependency would
/// buy nothing.
fn manifest_executable(install_dir: &Path, application_id: &str) -> Option<String> {
    let manifest = std::fs::read_to_string(install_dir.join("AppxManifest.xml")).ok()?;
    manifest_executable_from_xml(&manifest, application_id)
}

/// The text-scanning half of [`manifest_executable`], split out to be testable
/// without a real installed package.
fn manifest_executable_from_xml(manifest: &str, application_id: &str) -> Option<String> {
    for element in manifest.split("<Application").skip(1) {
        // Stop at the element's own closing bracket so a later element's
        // attributes can't leak into this one.
        let head = element.split('>').next().unwrap_or(element);
        if attribute(head, "Id").as_deref() != Some(application_id) {
            continue;
        }
        if let Some(executable) = attribute(head, "Executable") {
            return Some(executable);
        }
    }
    None
}

/// The value of `name="…"` within one element's attribute text.
fn attribute(element: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = element.find(&needle)? + needle.len();
    let rest = &element[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Truncates a null-terminated wide buffer and converts it to a `String`.
fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape ChatGPT's manifest actually has (2026-07-23):
    /// `<Application Id="App" Executable="app/ChatGPT.exe" …>`.
    #[test]
    fn manifest_executable_is_read_for_the_matching_application_id() {
        let xml = r#"<Package>
            <Applications>
                <Application Id="Helper" Executable="app/helper.exe" EntryPoint="X" />
                <Application Id="App" Executable="app/ChatGPT.exe" EntryPoint="Windows.FullTrustApplication">
                    <uap:VisualElements DisplayName="ChatGPT" />
                </Application>
            </Applications>
        </Package>"#;
        assert_eq!(
            manifest_executable_from_xml(xml, "App").as_deref(),
            Some("app/ChatGPT.exe")
        );
        assert_eq!(
            manifest_executable_from_xml(xml, "Helper").as_deref(),
            Some("app/helper.exe")
        );
        assert_eq!(manifest_executable_from_xml(xml, "Missing"), None);
    }

    #[test]
    fn attributes_of_a_later_element_do_not_leak_into_an_earlier_one() {
        // "App" declares no Executable; the next element's must not be used.
        let xml = r#"<Application Id="App" EntryPoint="X" />
                     <Application Id="Other" Executable="app/other.exe" />"#;
        assert_eq!(manifest_executable_from_xml(xml, "App"), None);
    }

    #[test]
    fn wide_to_string_stops_at_the_null_terminator() {
        let mut buf = [0u16; 8];
        for (i, c) in "abc".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(wide_to_string(&buf), "abc");
    }
}
