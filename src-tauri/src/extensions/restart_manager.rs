//! Shared Windows Restart Manager operations. Closed occupiers are not restarted.
use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        return Err(crate::utils::error::Error::Cancelled.into());
    }
    Ok(())
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

fn is_native_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["exe", "dll", "pyd"]
                .iter()
                .any(|native| extension.eq_ignore_ascii_case(native))
        })
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RmFileScope {
    NativeImages,
    /// Preserve the affected file range of an incremental transaction.
    AllFiles,
}

/// Collect native images from directories, and every explicitly supplied file.
/// Directory overlap must not discard explicit files with other extensions.
pub fn existing_files(
    paths: &[PathBuf],
    recursive: bool,
    cancelled: fn() -> bool,
) -> Result<Vec<PathBuf>> {
    collect_files(paths, recursive, false, &cancelled)
}

pub(crate) fn operation_files(
    paths: &[&Path],
    scope: RmFileScope,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<Vec<PathBuf>> {
    let files = collect_files(
        &paths.iter().map(|p| p.to_path_buf()).collect::<Vec<_>>(),
        true,
        scope == RmFileScope::AllFiles,
        cancelled,
    )?;
    check_cancelled(cancelled)?;
    Ok(files)
}

fn collect_files(
    paths: &[PathBuf],
    recursive: bool,
    all_files: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<PathBuf>> {
    let mut files = BTreeMap::new();
    let mut directories = Vec::new();
    for path in paths {
        check_cancelled(&cancelled)?;
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Unable to inspect {}", path.display()))
            }
        };
        if is_link(&metadata) {
            bail!(
                "Restart Manager resource is a link or junction: {}",
                path.display()
            );
        }
        if metadata.is_file() {
            files.insert(path_key(path), path.clone());
        } else if metadata.is_dir() {
            directories.push(path.clone());
        }
    }
    // Only collapse directories: explicit files are already retained above.
    directories.sort_by_key(|path| path.components().count());
    let mut roots = BTreeSet::new();
    directories.retain(|path| {
        let key = path_key(path);
        if roots.contains(&key)
            || (recursive
                && path
                    .ancestors()
                    .skip(1)
                    .any(|parent| roots.contains(&path_key(parent))))
        {
            false
        } else {
            roots.insert(key);
            true
        }
    });
    for path in directories {
        for entry in walkdir::WalkDir::new(path)
            .max_depth(if recursive { usize::MAX } else { 1 })
            .follow_links(false)
        {
            check_cancelled(&cancelled)?;
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_link(&metadata) {
                bail!(
                    "Restart Manager resource is a link or junction: {}",
                    entry.path().display()
                );
            }
            if metadata.is_file() && (all_files || is_native_file(entry.path())) {
                files.insert(path_key(entry.path()), entry.into_path());
            }
        }
    }
    Ok(files.into_values().collect())
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        os::windows::ffi::OsStrExt,
        ptr,
        sync::{Condvar, Mutex},
        time::Duration,
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME},
        System::{
            RestartManager::*,
            Threading::{GetCurrentProcess, GetProcessTimes},
        },
    };

    fn checked(code: u32, operation: &str) -> Result<()> {
        if code != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(code as i32))
                .with_context(|| format!("Restart Manager {operation} failed (Windows {code})"));
        }
        Ok(())
    }
    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }
    fn current_process() -> Result<RM_UNIQUE_PROCESS> {
        let (mut creation, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        if unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(RM_UNIQUE_PROCESS {
            dwProcessId: std::process::id(),
            ProcessStartTime: creation,
        })
    }

    pub struct Session {
        handle: u32,
    }
    impl Session {
        pub fn new(
            files: &[PathBuf],
            protected: &[PathBuf],
            cancelled: impl Fn() -> bool + Sync,
        ) -> Result<Self> {
            check_cancelled(&cancelled)?;
            let mut handle = 0;
            let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
            checked(
                unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) },
                "session start",
            )?;
            let session = Self { handle };
            for executable in protected {
                session.filter_executable(executable, RmNoShutdown)?;
            }
            // Protect this process last so caller filters cannot override it.
            let current = current_process().context("Unable to protect the current process")?;
            session.filter_process(&current, RmNoShutdown)?;
            session.filter_executable(&std::env::current_exe()?, RmNoShutdown)?;
            // Register in batches: each registration has a registry write cost.
            for batch in files.chunks(256) {
                check_cancelled(&cancelled)?;
                let names: Vec<_> = batch.iter().map(|path| wide(path)).collect();
                let pointers: Vec<_> = names.iter().map(|name| name.as_ptr()).collect();
                checked(
                    unsafe {
                        RmRegisterResources(
                            handle,
                            pointers.len() as u32,
                            pointers.as_ptr(),
                            0,
                            ptr::null(),
                            0,
                            ptr::null(),
                        )
                    },
                    "file registration",
                )?;
            }
            Ok(session)
        }

        pub fn occupiers(&self) -> Result<Vec<String>> {
            let (affected, reasons) = self.list()?;
            let pids: Vec<_> = affected
                .iter()
                .map(|p| sysinfo::Pid::from_u32(p.Process.dwProcessId))
                .collect();
            let mut system = sysinfo::System::new();
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
            let values = affected
                .into_iter()
                .map(|p| {
                    let end = p
                        .strAppName
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(p.strAppName.len());
                    let name = String::from_utf16_lossy(&p.strAppName[..end]);
                    let exe = system
                        .process(sysinfo::Pid::from_u32(p.Process.dwProcessId))
                        .and_then(|p| p.exe())
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "unknown".into());
                    format!(
                        "PID={} name={} exe={} status={} reboot_reasons={}",
                        p.Process.dwProcessId, name, exe, p.AppStatus, reasons
                    )
                })
                .collect();
            Ok(values)
        }

        pub fn shutdown(&mut self, force: bool, cancelled: impl Fn() -> bool + Sync) -> Result<()> {
            check_cancelled(&cancelled)?;
            let (affected, reasons) = self.list()?;
            self.check_occupiers(&affected, reasons)?;
            if !affected.is_empty() {
                checked(self.request_shutdown(force, &cancelled), "shutdown")?;
                check_cancelled(&cancelled)?;
                let (remaining, reasons) = self.list()?;
                self.check_occupiers(&remaining, reasons)?;
                for process in remaining {
                    if process.AppStatus & RmStatusRunning as u32 != 0 {
                        bail!("Restart Manager could not close every file occupier");
                    }
                }
            }
            Ok(())
        }

        fn filter_process(
            &self,
            process: &RM_UNIQUE_PROCESS,
            policy: RM_FILTER_ACTION,
        ) -> Result<()> {
            checked(
                unsafe { RmAddFilter(self.handle, ptr::null(), process, ptr::null(), policy) },
                "process protection",
            )
        }
        fn filter_executable(&self, executable: &Path, policy: RM_FILTER_ACTION) -> Result<()> {
            checked(
                unsafe {
                    RmAddFilter(
                        self.handle,
                        wide(executable).as_ptr(),
                        ptr::null(),
                        ptr::null(),
                        policy,
                    )
                },
                "application policy",
            )
        }
        fn check_occupiers(&self, processes: &[RM_PROCESS_INFO], reasons: u32) -> Result<()> {
            if reasons != 0 {
                return Err(std::io::Error::from_raw_os_error(3010)).with_context(|| {
                    format!("Restart Manager requires a system restart (reasons {reasons})")
                });
            }
            if processes.iter().any(|process| {
                process.Process.dwProcessId == std::process::id()
                    || process.AppStatus & RmStatusShutdownMasked as u32 != 0
            }) {
                bail!("Restart Manager found a protected process using registered files");
            }
            Ok(())
        }
        fn list(&self) -> Result<(Vec<RM_PROCESS_INFO>, u32)> {
            let mut processes = Vec::<RM_PROCESS_INFO>::new();
            for _ in 0..3 {
                let (mut needed, mut count, mut reasons) = (0, processes.len() as u32, 0);
                let buffer = if processes.is_empty() {
                    ptr::null_mut()
                } else {
                    processes.as_mut_ptr()
                };
                let code = unsafe {
                    RmGetList(self.handle, &mut needed, &mut count, buffer, &mut reasons)
                };
                if code == ERROR_SUCCESS {
                    processes.truncate(count as usize);
                    return Ok((processes, reasons));
                }
                if code != ERROR_MORE_DATA {
                    checked(code, "occupancy query")?;
                }
                processes.resize(needed as usize, RM_PROCESS_INFO::default());
            }
            bail!("Restart Manager occupancy changed repeatedly")
        }
        fn request_shutdown(&self, force: bool, cancelled: &(dyn Fn() -> bool + Sync)) -> u32 {
            // RM is synchronous. This watcher requests cancellation without
            // terminating occupiers, and is joined before returning the session.
            let done = Mutex::new(false);
            let wake = Condvar::new();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let mut finished = done.lock().unwrap();
                    while !*finished {
                        if cancelled() {
                            drop(finished);
                            unsafe {
                                RmCancelCurrentTask(self.handle);
                            }
                            break;
                        }
                        finished = wake
                            .wait_timeout(finished, Duration::from_millis(100))
                            .unwrap()
                            .0;
                    }
                });
                let flags = if force { RmForceShutdown as u32 } else { 0 };
                let result = unsafe { RmShutdown(self.handle, flags, None) };
                *done.lock().unwrap() = true;
                wake.notify_one();
                result
            })
        }
    }
    impl Drop for Session {
        fn drop(&mut self) {
            unsafe {
                RmEndSession(self.handle);
            }
        }
    }
}

#[cfg(windows)]
pub use windows::Session;

#[cfg(not(windows))]
pub struct Session;
#[cfg(not(windows))]
impl Session {
    pub fn new(_: &[PathBuf], _: &[PathBuf], cancelled: impl Fn() -> bool + Sync) -> Result<Self> {
        check_cancelled(&cancelled)?;
        bail!("File occupancy handling requires Windows Restart Manager")
    }
    pub fn occupiers(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }
    pub fn shutdown(&mut self, _: bool, cancelled: impl Fn() -> bool + Sync) -> Result<()> {
        check_cancelled(&cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        parent: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/pyappify-lab/restart-manager-native-files/collector-fixtures");
            fs::create_dir_all(&parent).unwrap();
            let parent = fs::canonicalize(parent).unwrap();
            let root = parent.join(format!("case-{:x}", rand::random::<u64>()));
            fs::create_dir(&root).unwrap();
            Self { parent, root }
        }

        fn write(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"fixture").unwrap();
            path
        }

        fn collect(&self, paths: &[PathBuf]) -> BTreeSet<String> {
            let files = existing_files(paths, true, || false).unwrap();
            let names: BTreeSet<_> = files
                .iter()
                .map(|path| {
                    path.strip_prefix(&self.root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect();
            assert_eq!(files.len(), names.len(), "Resources must be deduplicated");
            names
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let root = fs::canonicalize(&self.root).unwrap();
            assert!(root.is_absolute() && root.parent() == Some(self.parent.as_path()));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn operation_scope_preserves_explicit_files_and_delta_registration() {
        let fixture = Fixture::new();
        let native = fixture.write("deep/nested/module.Cp312.PyD");
        let explicit = fixture.write("deep/nested/settings.json");
        let source = fixture.write("main.py");
        let nested = fixture.root.join("deep");
        let collect = |paths: &[&Path], scope| {
            operation_files(paths, scope, &|| false)
                .unwrap()
                .into_iter()
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            collect(&[&fixture.root], RmFileScope::NativeImages),
            BTreeSet::from([native.clone()])
        );
        for paths in [
            vec![fixture.root.as_path(), &nested, &explicit, &explicit],
            vec![explicit.as_path(), &nested, &fixture.root],
        ] {
            let files = operation_files(&paths, RmFileScope::NativeImages, &|| false).unwrap();
            assert_eq!(files.len(), 2, "Resources must be deduplicated");
            assert_eq!(
                files.into_iter().collect::<BTreeSet<_>>(),
                BTreeSet::from([native.clone(), explicit.clone()])
            );
        }
        assert_eq!(
            collect(&[&fixture.root], RmFileScope::AllFiles),
            BTreeSet::from([native, explicit, source])
        );
        // Cancellation may be captured from the caller's task state.
        let cancelled = true;
        assert!(
            operation_files(&[&fixture.root], RmFileScope::NativeImages, &|| cancelled).is_err()
        );
    }

    #[test]
    fn directories_collect_native_images_at_every_depth_case_insensitively() {
        let fixture = Fixture::new();
        let expected = [
            "bin/Product.ExE",
            "python/deep/site-packages/native.Cp312.PyD",
            "python/deep/deeper/lib.DLL",
        ];
        for name in expected {
            fixture.write(name);
        }
        for name in [
            "app.json",
            "pyappify.yml",
            "python/readme.txt",
            "data/file.exe.config",
            "cache.dll/settings.ini",
        ] {
            fixture.write(name);
        }
        assert_eq!(
            fixture.collect(&[fixture.root.clone()]),
            expected.into_iter().map(String::from).collect()
        );
    }

    #[test]
    fn explicitly_supplied_files_are_not_filtered_by_extension() {
        let fixture = Fixture::new();
        let expected = [
            "app.json",
            ".private-state.json",
            "pyappify.yml",
            "payload.bin",
        ];
        let mut paths: Vec<_> = expected.iter().map(|name| fixture.write(name)).collect();
        paths.push(fixture.root.join("missing.json"));
        assert_eq!(
            fixture.collect(&paths),
            expected.into_iter().map(String::from).collect()
        );
    }

    #[test]
    fn overlapping_directories_preserve_explicit_files_in_either_input_order() {
        let fixture = Fixture::new();
        let json = fixture.write("state/deep/app.json");
        let native = fixture.write("state/deep/native.dll");
        fixture.write("state/deep/implicit.json");
        let mut paths = vec![
            fixture.root.clone(),
            fixture.root.join("state"),
            json.clone(),
            json,
            native,
            fixture.root.clone(),
        ];
        let expected: BTreeSet<_> = ["state/deep/app.json", "state/deep/native.dll"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(fixture.collect(&paths), expected);
        paths.reverse();
        assert_eq!(fixture.collect(&paths), expected);
    }
}
