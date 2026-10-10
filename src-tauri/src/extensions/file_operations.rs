//! Opt-in file operations for added features. Existing upstream callers stay unchanged.
use super::restart_manager::{self, RmFileScope};
use anyhow::{bail, Context, Result};
use std::{
    fs, io,
    io::Read,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq)]
pub enum DeletePolicy {
    PreserveAttributes,
    /// Caller has established that the tree is disposable, or owns its rollback.
    Disposable,
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
        || path.parent().is_none()
    {
        bail!("Unsafe file operation target: {}", path.display());
    }
    // Refuse links/junctions in either the target or its existing ancestors.
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if is_reparse(&metadata) => bail!(
                "File operation target contains a link or junction: {}",
                ancestor.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Unable to inspect {}", ancestor.display()))
            }
        }
    }
    Ok(path)
}

fn retryable(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(5 | 32 | 33 | 145 | 1224))
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

// The helper permits offline sequence tests without invoking native RM.
fn attempt_then_release(
    cancelled: &dyn Fn() -> bool,
    action: &mut impl FnMut() -> io::Result<()>,
    release: &mut impl FnMut(&io::Error) -> Result<()>,
) -> Result<()> {
    if cancelled() {
        return Err(crate::utils::error::Error::Cancelled.into());
    }
    let first = match action() {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    if !retryable(&first) {
        return Err(first.into());
    }
    if cancelled() {
        return Err(crate::utils::error::Error::Cancelled.into());
    }
    let released = release(&first);
    if cancelled() {
        return Err(crate::utils::error::Error::Cancelled.into());
    }
    action().with_context(|| {
        format!(
            "First error: {first}; RM: {}",
            match released {
                Ok(()) => "completed".into(),
                Err(error) => format!("{error:#}"),
            }
        )
    })
}

fn run(
    operation: &str,
    purpose: &str,
    paths: &[&Path],
    cancelled: &(dyn Fn() -> bool + Sync),
    rm_scope: RmFileScope,
    mut action: impl FnMut() -> io::Result<()>,
) -> Result<()> {
    let mut occupants = String::new();
    let result = attempt_then_release(cancelled, &mut action, &mut |first| {
        tracing::warn!(operation, purpose, paths=?paths, windows_error=?first.raw_os_error(), %first,
            "File operation failed; trying Restart Manager before one retry");
        occupants = "未识别占用进程".into();
        let result = (|| {
            let files = restart_manager::operation_files(paths, rm_scope, cancelled)?;
            if files.is_empty() {
                return Ok(());
            }
            let mut session = restart_manager::Session::new(&files, &[], cancelled)?;
            let affected = session.occupiers()?;
            if !affected.is_empty() {
                occupants = affected.join("; ");
            }
            session.shutdown(true, cancelled)
        })();
        // Keep RM diagnostics even when the subsequent file retry succeeds.
        tracing::info!(operation, purpose, %occupants, result=?result, "Restart Manager result");
        result
    });
    if let Err(error) = result {
        // Cancellation and validation errors must not cause another occupancy query.
        if let Some(code) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<io::Error>())
            .map(|e| e.raw_os_error())
        {
            let message = format!(
                "{operation} failed ({purpose}), paths={paths:?}, os_error={code:?}; {}",
                if occupants.is_empty() {
                    "占用进程未识别或查询未执行"
                } else {
                    &occupants
                }
            );
            tracing::error!(operation, purpose, paths=?paths, windows_error=?code, error=%format!("{error:#}"), %occupants, "File operation failed after recovery attempt");
            return Err(error.context(message));
        }
        return Err(error);
    }
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let path = absolute(path)?;
    let temporary = path.with_extension(format!("{:x}.tmp", rand::random::<u64>()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, &path, "Atomic file publication", &|| false)
    })();
    if result.is_err() {
        let _ = remove_file(&temporary, "Failed atomic write cleanup", &|| false);
    }
    result
}

/// Move to an empty destination; never copy/delete as a fallback or overwrite it.
pub fn move_path(
    source: &Path,
    destination: &Path,
    purpose: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    move_inner(
        source,
        destination,
        false,
        purpose,
        cancelled,
        RmFileScope::NativeImages,
    )
}

pub(crate) fn move_path_with_rm_scope(
    source: &Path,
    destination: &Path,
    purpose: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
    rm_scope: RmFileScope,
) -> Result<()> {
    move_inner(source, destination, false, purpose, cancelled, rm_scope)
}

/// Atomically publish a closed, flushed temporary file over another file.
pub fn replace_file(
    source: &Path,
    destination: &Path,
    purpose: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    move_inner(
        source,
        destination,
        true,
        purpose,
        cancelled,
        RmFileScope::NativeImages,
    )
}

fn move_inner(
    source: &Path,
    destination: &Path,
    replace: bool,
    purpose: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
    rm_scope: RmFileScope,
) -> Result<()> {
    let source = absolute(source)?;
    let destination = absolute(destination)?;
    let action = || {
        if replace {
            if !fs::symlink_metadata(&source)?.is_file() || destination.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Atomic replacement requires files",
                ));
            }
        } else {
            match fs::symlink_metadata(&destination) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Move destination already exists",
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            };
            let wide = |path: &Path| {
                path.as_os_str()
                    .encode_wide()
                    .chain(Some(0))
                    .collect::<Vec<_>>()
            };
            let flags = MOVEFILE_WRITE_THROUGH
                | if replace {
                    MOVEFILE_REPLACE_EXISTING
                } else {
                    0
                };
            if unsafe { MoveFileExW(wide(&source).as_ptr(), wide(&destination).as_ptr(), flags) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(windows))]
        fs::rename(&source, &destination)
    };
    run(
        "move",
        purpose,
        &[&source, &destination],
        cancelled,
        rm_scope,
        action,
    )
}

fn validate_tree(
    path: &Path,
    clear_readonly: bool,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    if cancelled() {
        return Err(crate::utils::error::Error::Cancelled.into());
    }
    if !path.exists() {
        return Ok(());
    }
    let mut readonly = Vec::new();
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
    {
        if cancelled() {
            return Err(crate::utils::error::Error::Cancelled.into());
        }
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_reparse(&metadata) {
            bail!(
                "Deletion tree contains a link or junction: {}",
                entry.path().display()
            );
        }
        if clear_readonly && metadata.is_file() && metadata.permissions().readonly() {
            readonly.push(entry.into_path());
        }
    }
    #[cfg(windows)]
    for file in readonly {
        if cancelled() {
            return Err(crate::utils::error::Error::Cancelled.into());
        }
        let metadata = fs::symlink_metadata(&file)?;
        if is_reparse(&metadata) {
            bail!("Deletion target became a link: {}", file.display());
        }
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        fs::set_permissions(file, permissions)?;
    }
    Ok(())
}

pub fn remove_tree(
    path: &Path,
    policy: DeletePolicy,
    purpose: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    let path = absolute(path)?;
    validate_tree(&path, policy == DeletePolicy::Disposable, cancelled)?;
    run(
        "delete tree",
        purpose,
        &[&path],
        cancelled,
        RmFileScope::NativeImages,
        || match fs::remove_dir_all(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        },
    )
}

pub fn remove_file(
    path: &Path,
    purpose: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    let path = absolute(path)?;
    run(
        "delete file",
        purpose,
        &[&path],
        cancelled,
        RmFileScope::NativeImages,
        || match fs::remove_file(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        },
    )
}

/// Nonempty directories are retained. Never escalates an empty-dir cleanup to a tree delete.
pub fn remove_empty_dir(
    path: &Path,
    purpose: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    remove_empty_dir_with_rm_scope(path, purpose, cancelled, RmFileScope::NativeImages)
}

pub(crate) fn remove_empty_dir_with_rm_scope(
    path: &Path,
    purpose: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
    rm_scope: RmFileScope,
) -> Result<()> {
    let path = absolute(path)?;
    run(
        "delete empty directory",
        purpose,
        &[&path],
        cancelled,
        rm_scope,
        || match fs::remove_dir(&path) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                Ok(())
            }
            result => result,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn successful_and_permanent_operations_never_call_rm() {
        let releases = Cell::new(0);
        let mut release = |_: &io::Error| {
            releases.set(releases.get() + 1);
            Ok(())
        };
        attempt_then_release(&|| false, &mut || Ok(()), &mut release).unwrap();
        assert!(attempt_then_release(
            &|| false,
            &mut || Err(io::Error::from_raw_os_error(2)),
            &mut release
        )
        .is_err());
        assert_eq!(releases.get(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn failure_calls_rm_then_retries_even_when_rm_failed() {
        let calls = Cell::new(0);
        let released = Cell::new(false);
        attempt_then_release(
            &|| false,
            &mut || {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    Err(io::Error::from_raw_os_error(5))
                } else {
                    assert!(released.get());
                    Ok(())
                }
            },
            &mut |_| {
                released.set(true);
                bail!("RM fixture error")
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        calls.set(0);
        let error = attempt_then_release(
            &|| false,
            &mut || {
                calls.set(calls.get() + 1);
                Err(io::Error::from_raw_os_error(32))
            },
            &mut |_| bail!("RM fixture error"),
        )
        .unwrap_err();
        assert_eq!(calls.get(), 2);
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
            Some(32)
        );
        assert!(format!("{error:#}").contains("RM fixture error"));
    }

    #[test]
    fn cancellation_prevents_mutation_and_retry() {
        let calls = Cell::new(0);
        let cancelled = Cell::new(true);
        let mut action = || {
            calls.set(calls.get() + 1);
            Err(io::Error::from_raw_os_error(5))
        };
        assert!(attempt_then_release(&|| cancelled.get(), &mut action, &mut |_| Ok(())).is_err());
        assert_eq!(calls.get(), 0);
        cancelled.set(false);
        assert!(
            attempt_then_release(&|| cancelled.get(), &mut action, &mut |_| {
                cancelled.set(true);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn moves_preserve_collisions_and_cleanup_keeps_nonempty_directories() {
        let root =
            std::env::temp_dir().join(format!("pyappify-file-ops-{:x}", rand::random::<u64>()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source");
        let target = root.join("target");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("body"), b"original").unwrap();
        move_path(&source, &target, "fixture commit", &|| false).unwrap();
        assert!(!source.exists());
        fs::create_dir(&source).unwrap();
        assert!(move_path(&source, &target, "fixture collision", &|| false).is_err());
        assert_eq!(fs::read(target.join("body")).unwrap(), b"original");
        remove_empty_dir(&target, "fixture empty cleanup", &|| false).unwrap();
        assert!(target.exists());
        remove_tree(
            &target,
            DeletePolicy::Disposable,
            "fixture cleanup",
            &|| false,
        )
        .unwrap();
        remove_tree(
            &target,
            DeletePolicy::Disposable,
            "fixture missing cleanup",
            &|| false,
        )
        .unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn atomic_publication_replaces_files_but_never_directories() {
        let root =
            std::env::temp_dir().join(format!("pyappify-publication-{:x}", rand::random::<u64>()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("temporary");
        let destination = root.join("state.json");
        fs::write(&source, b"new").unwrap();
        fs::write(&destination, b"old").unwrap();
        replace_file(&source, &destination, "fixture publication", &|| false).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        fs::create_dir(&source).unwrap();
        assert!(
            replace_file(&source, &destination, "fixture refuse directory", &|| false).is_err()
        );
        assert!(source.is_dir());
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert!(move_path(
            &destination,
            &root.join("../outside"),
            "fixture refuse traversal",
            &|| false
        )
        .is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn readonly_changes_require_disposable_policy_and_cannot_run_after_cancellation() {
        let root =
            std::env::temp_dir().join(format!("pyappify-disposable-{:x}", rand::random::<u64>()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("readonly");
        fs::write(&file, b"body").unwrap();
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).unwrap();
        validate_tree(&root, false, &|| false).unwrap();
        assert!(fs::metadata(&file).unwrap().permissions().readonly());
        assert!(remove_tree(
            &root,
            DeletePolicy::Disposable,
            "fixture cancelled cleanup",
            &|| true
        )
        .is_err());
        assert!(fs::metadata(&file).unwrap().permissions().readonly());
        remove_tree(
            &root,
            DeletePolicy::Disposable,
            "fixture disposable cleanup",
            &|| false,
        )
        .unwrap();
        assert!(!root.exists());
    }
}

pub(crate) fn relative(value: &str) -> Result<PathBuf> {
    if value.is_empty() || value.contains('\\') {
        bail!("Invalid ZIP path: {value}");
    }
    let mut path = PathBuf::new();
    for component in value.split('/') {
        if component.is_empty()
            || matches!(component, "." | "..")
            || component.ends_with(['.', ' '])
            || component.chars().any(|c| c < ' ' || "<>:\"|?*".contains(c))
        {
            bail!("Invalid ZIP path: {value}");
        }
        let stem = component.split('.').next().unwrap().to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|n| {
                    matches!(
                        n,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            })
        {
            bail!("Reserved Windows ZIP path: {value}");
        }
        path.push(component);
    }
    Ok(path)
}

pub(crate) fn key(value: &str) -> String {
    value.to_lowercase()
}

pub(crate) fn within(value: &str, parent: &str) -> bool {
    let value = key(value);
    let parent = key(parent);
    value == parent || value.starts_with(&(parent + "/"))
}

pub(crate) fn is_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

/// Do not follow existing junctions or links during replacement or rollback.
pub(crate) fn safe_join(root: &Path, value: &str) -> Result<PathBuf> {
    let relative = relative(value)?;
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if is_reparse(&metadata) => {
                bail!("ZIP destination contains a link or junction: {value}")
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

pub(crate) fn check_cancelled(cancelled: &impl Fn() -> bool) -> Result<()> {
    if cancelled() {
        return Err(crate::utils::error::Error::Cancelled.into());
    }
    Ok(())
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    reader: impl Read,
    limit: u64,
) -> Result<T> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("Operation metadata is too large");
    }
    Ok(serde_json::from_slice(&bytes).context("Invalid operation metadata")?)
}
