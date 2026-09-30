//! Native Windows Jump List integration for Warp OSS.
//!
//! The list contains one task per shell that Warp discovered. Each task starts
//! the current executable with a `warposs://` URI, so the shell selection is
//! handled by Warp itself rather than by an intermediate terminal emulator.

use std::path::{Path, PathBuf};

use url::Url;
use warpui::{AppContext, SingletonEntity as _};
use windows::Win32::Storage::EnhancedStorage::PKEY_Title;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::UI::Shell::Common::IObjectCollection;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{DestinationList, ICustomDestinationList, IShellLinkW, ShellLink};
use windows::core::{Interface, PCWSTR, Result};

use crate::ChannelState;
use crate::terminal::available_shells::{AvailableShell, AvailableShells};

#[derive(Clone, Debug, Eq, PartialEq)]
struct JumpListProfile {
    title: String,
    shell_id: String,
    icon_path: Option<PathBuf>,
}

/// Refresh the task list after Warp has finished discovering its shells.
///
/// COM is initialized on a dedicated STA thread. A Jump List failure is
/// intentionally isolated from app startup: the feature is useful, but it
/// must never prevent Warp from launching.
pub(crate) fn update(ctx: &mut AppContext) {
    let shells = AvailableShells::as_ref(ctx);
    let profiles = shells
        .get_available_shells()
        .filter_map(|shell| {
            shell.get_valid_shell_path_and_type()?;

            let shell_id = shell.id()?.to_owned();
            let title = profile_title(&shells, shell);

            // `cmd.exe` is not one of Warp's supported Windows shells. Keep
            // this guard here as well so a future discovery change cannot add
            // it to the native launch surface accidentally.
            let details = shell.details().to_ascii_lowercase();
            if details == "cmd.exe" || details.ends_with("\\cmd.exe") {
                return None;
            }

            Some(JumpListProfile {
                title,
                shell_id,
                icon_path: shell_icon_path(shell),
            })
        })
        .collect::<Vec<_>>();

    let mut profiles = profiles;
    profiles.sort_by(|left, right| {
        left.title
            .cmp(&right.title)
            .then_with(|| left.shell_id.cmp(&right.shell_id))
    });
    profiles.dedup_by(|left, right| left.shell_id.eq_ignore_ascii_case(&right.shell_id));

    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            log::warn!("Could not determine Warp executable for Jump List: {error}");
            return;
        }
    };
    let app_id = ChannelState::app_id().to_string();
    let url_scheme = ChannelState::url_scheme().to_owned();

    let spawn_result = std::thread::Builder::new()
        .name("warp-jumplist".to_owned())
        .spawn(move || {
            if let Err(error) = write_jump_list(&executable, &app_id, &url_scheme, &profiles) {
                log::warn!("Could not update Warp Windows Jump List: {error}");
            }
        });

    if let Err(error) = spawn_result {
        log::warn!("Could not start Warp Jump List update: {error}");
    }
}

fn profile_title(shells: &AvailableShells, shell: &AvailableShell) -> String {
    let short_name = shell.short_name();
    let details = shell.details();
    let details_lower = details.to_ascii_lowercase();

    if shell.is_wsl() {
        return short_name.into_owned();
    }

    if short_name == "PowerShell" && details_lower.ends_with("\\pwsh.exe") {
        return "PowerShell 7".to_owned();
    }

    if short_name == "Windows PowerShell" {
        return "PowerShell 5".to_owned();
    }

    if matches!(short_name.as_ref(), "Bash" | "bash" | "bash.exe")
        && details_lower.contains("\\git\\")
    {
        return "Git Bash".to_owned();
    }

    shells.display_name_for_shell(shell).into_owned()
}

fn shell_icon_path(shell: &AvailableShell) -> Option<PathBuf> {
    if shell.is_wsl() {
        let distro = shell.wsl_distro()?;
        let distro_key = distro.to_ascii_lowercase();
        let mut candidates = vec![format!("{distro_key}.exe")];

        // Store launchers commonly use compact names for versioned Ubuntu
        // distributions, for example ubuntu2204.exe.
        if let Some(version) = distro_key
            .strip_prefix("ubuntu-")
            .or_else(|| distro_key.strip_prefix("ubuntu "))
        {
            candidates.push(format!("ubuntu{}.exe", version.replace('.', "")));
        }
        candidates.push("ubuntu.exe".to_owned());
        candidates.push("wsl.exe".to_owned());

        return candidates.into_iter().find_map(|candidate| find_on_path(&candidate));
    }

    let details = shell.details();
    let path = PathBuf::from(details.to_string());

    // Discovery uses usr\bin\bash.exe; other installations use bin\bash.exe.
    // Resolve the installation root using the launcher, then prefer the same
    // icon as the official Git Bash Start Menu shortcut.
    if path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("bash.exe"))
        && path
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("\\git\\")
    {
        for git_root in path.ancestors().skip(1).take(3) {
            let launcher = git_root.join("git-bash.exe");
            if launcher.is_file() {
                for relative in [
                    "mingw64/share/git/git-for-windows.ico",
                    "mingw32/share/git/git-for-windows.ico",
                ] {
                    let icon = git_root.join(relative);
                    if icon.is_file() {
                        return Some(icon);
                    }
                }
                return Some(launcher);
            }
        }
    }

    path.is_file().then_some(path)
}

fn find_on_path(file_name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_string_lossy().split(';').find_map(|directory| {
        let candidate = Path::new(directory).join(file_name);
        candidate.is_file().then_some(candidate)
    })
}

fn task_uri(scheme: &str, shell_id: &str) -> String {
    let mut uri = Url::parse(&format!("{scheme}://action/new_window"))
        .expect("the channel URL scheme must produce a valid URI");
    uri.query_pairs_mut().append_pair("shell", shell_id);
    // Change the task identity when icon metadata changes so Explorer does
    // not reuse an older cached Jump List link.
    uri.query_pairs_mut().append_pair("icon_revision", "2");
    uri.to_string()
}

fn wide(value: impl AsRef<str>) -> Vec<u16> {
    value.as_ref().encode_utf16().chain(Some(0)).collect()
}

fn write_jump_list(
    executable: &Path,
    app_id: &str,
    url_scheme: &str,
    profiles: &[JumpListProfile],
) -> Result<()> {
    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if let Err(error) = initialized.ok() {
        return Err(error);
    }

    let result = write_jump_list_on_initialized_thread(executable, app_id, url_scheme, profiles);
    unsafe { CoUninitialize() };
    result
}

fn write_jump_list_on_initialized_thread(
    executable: &Path,
    app_id: &str,
    url_scheme: &str,
    profiles: &[JumpListProfile],
) -> Result<()> {
    unsafe {
        let destination_list: ICustomDestinationList =
            CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;

        let app_id_wide = wide(app_id);
        destination_list.SetAppID(PCWSTR(app_id_wide.as_ptr()))?;

        let mut minimum_slots = 0;
        let tasks: IObjectCollection = destination_list.BeginList(&mut minimum_slots)?;

        let result = (|| {
            let executable_string = executable.to_string_lossy();
            let executable_wide = wide(&*executable_string);

            for profile in profiles {
                let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
                let title_wide = wide(&profile.title);
                let arguments_wide = wide(task_uri(url_scheme, &profile.shell_id));

                link.SetPath(PCWSTR(executable_wide.as_ptr()))?;
                link.SetArguments(PCWSTR(arguments_wide.as_ptr()))?;
                link.SetDescription(PCWSTR(title_wide.as_ptr()))?;

                let property_store: IPropertyStore = link.cast()?;
                let title_property: PROPVARIANT = profile.title.as_str().into();
                property_store.SetValue(&PKEY_Title, &title_property)?;
                property_store.Commit()?;

                // Set the icon after committing the property store. Explorer
                // can otherwise retain the target executable's icon when the
                // link is serialized into the Jump List.
                let icon_path = profile
                    .icon_path
                    .as_deref()
                    .unwrap_or(executable);
                let icon_path_string = icon_path.to_string_lossy();
                let icon_path_wide = wide(&*icon_path_string);
                link.SetIconLocation(PCWSTR(icon_path_wide.as_ptr()), 0)?;

                let unknown = link.cast::<windows::core::IUnknown>()?;
                tasks.AddObject(&unknown)?;
            }

            destination_list.AddUserTasks(&tasks)?;
            destination_list.CommitList()
        })();

        if result.is_err() {
            let _ = destination_list.AbortList();
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::task_uri;
    use url::Url;

    #[test]
    fn task_uri_percent_encodes_shell_identifiers() {
        let uri = task_uri("warposs", r"local:C:\Program Files\PowerShell\7\pwsh.exe");
        let parsed = Url::parse(&uri).unwrap();

        assert_eq!(parsed.scheme(), "warposs");
        assert_eq!(parsed.path(), "/new_window");
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "shell")
                .map(|(_, value)| value.into_owned()),
            Some(r"local:C:\Program Files\PowerShell\7\pwsh.exe".to_owned())
        );
        assert!(uri.contains('+'));
        assert!(uri.contains("%5C"));
    }
}
