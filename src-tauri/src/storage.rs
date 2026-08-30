use std::io;
use std::path::{Path, PathBuf};

const APP_DATA_DIR_NAME: &str = "VRChatDraw";

/// Return the per-user directory used for mutable application data.
///
/// The old application wrote beside the executable, which is commonly read-only
/// under an installed Windows location.  Keep the fallback deterministic for
/// portable/test environments, but prefer the platform user config directory.
pub fn app_data_dir() -> PathBuf {
    #[cfg(test)]
    {
        std::env::temp_dir().join(format!("{APP_DATA_DIR_NAME}-test-{}", std::process::id()))
    }

    #[cfg(not(test))]
    {
        #[cfg(windows)]
        {
            if let Some(base) = absolute_env_path("APPDATA") {
                return base.join(APP_DATA_DIR_NAME);
            }
        }

        #[cfg(not(windows))]
        {
            if let Some(base) = absolute_env_path("XDG_CONFIG_HOME") {
                return base.join(APP_DATA_DIR_NAME);
            }
            if let Some(home) = absolute_env_path("HOME") {
                return home.join(".config").join(APP_DATA_DIR_NAME);
            }
        }

        std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_DATA_DIR_NAME)
    }
}

#[cfg(not(test))]
fn absolute_env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

pub fn ensure_app_data_dir() -> Result<PathBuf, String> {
    let directory = app_data_dir();
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("无法创建应用数据目录 {directory:#?}: {error}"))?;
    Ok(directory)
}

pub fn data_path(file_name: &str) -> PathBuf {
    app_data_dir().join(file_name)
}

/// Path used by releases before user-data storage was introduced.
pub fn legacy_sidecar_path(file_name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|directory| directory.join(file_name)))
        .unwrap_or_else(|| PathBuf::from(file_name))
}

/// Read the new location first and fall back to the legacy executable-side file
/// only when the new location does not exist.
pub fn read_preferred(file_name: &str) -> io::Result<Option<(PathBuf, Vec<u8>)>> {
    let primary = data_path(file_name);
    match std::fs::read(&primary) {
        Ok(bytes) => Ok(Some((primary, bytes))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let legacy = legacy_sidecar_path(file_name);
            match std::fs::read(&legacy) {
                Ok(bytes) => Ok(Some((legacy, bytes))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Preserve a malformed file before the caller starts with defaults.  Renaming
/// keeps the original bytes available for diagnosis and prevents a later save
/// from silently destroying the only copy of the user's settings/history.
pub fn preserve_corrupt(path: &Path) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let backup = path.with_file_name(format!(
        "{file_name}.corrupt.{}.{}",
        std::process::id(),
        monotonic_suffix()
    ));
    std::fs::rename(path, &backup)?;
    Ok(backup)
}

/// Write a file through a process-unique temporary file and replace the target.
///
/// Unix rename replaces an existing file directly.  Windows does not, so the
/// fallback moves the old target aside, installs the new file, and restores the
/// old target if the second move fails.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建数据目录 {parent:#?}: {error}"))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let tmp = path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        monotonic_suffix()
    ));
    if let Err(error) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("写入 {path:#?} 失败：{error}"));
    }

    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            if !path.exists() {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("替换 {path:#?} 失败：{first_error}"));
            }

            let backup = path.with_file_name(format!(
                ".{file_name}.bak.{}.{}",
                std::process::id(),
                monotonic_suffix()
            ));
            if let Err(error) = std::fs::rename(path, &backup) {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("替换 {path:#?} 失败：{error}"));
            }

            match std::fs::rename(&tmp, path) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&backup);
                    Ok(())
                }
                Err(error) => {
                    let _ = std::fs::rename(&backup, path);
                    let _ = std::fs::remove_file(&tmp);
                    Err(format!("替换 {path:#?} 失败：{error}"))
                }
            }
        }
    }
}

fn monotonic_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_existing_file() {
        let directory = std::env::temp_dir().join(format!(
            "vrc_storage_test_{}_{}",
            std::process::id(),
            monotonic_suffix()
        ));
        let path = directory.join("value.json");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(directory);
    }
}
