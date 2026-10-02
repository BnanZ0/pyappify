//! Shared Windows Restart Manager operations. Callers own shutdown/restart policy.
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn check_cancelled(cancelled: fn() -> bool) -> Result<()> {
    if cancelled() {
        bail!("Operation cancelled by user");
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

/// Enumerate existing files; the caller chooses roots and traversal depth.
pub fn existing_files(
    paths: &[PathBuf],
    recursive: bool,
    cancelled: fn() -> bool,
) -> Result<Vec<PathBuf>> {
    let mut files = BTreeMap::new();
    for path in paths {
        check_cancelled(cancelled)?;
        match fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Unable to inspect {}", path.display()))
            }
        }
        for entry in walkdir::WalkDir::new(path)
            .max_depth(if recursive { usize::MAX } else { 1 })
            .follow_links(false)
        {
            check_cancelled(cancelled)?;
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_link(&metadata) {
                bail!(
                    "Restart Manager resource is a link or junction: {}",
                    entry.path().display()
                );
            }
            if metadata.is_file() {
                files.insert(
                    entry.path().to_string_lossy().to_lowercase(),
                    entry.into_path(),
                );
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
        restart_needed: bool,
    }
    impl Session {
        pub fn new(
            files: &[PathBuf],
            no_restart: &[PathBuf],
            protected: &[PathBuf],
            cancelled: fn() -> bool,
        ) -> Result<Self> {
            check_cancelled(cancelled)?;
            let mut handle = 0;
            let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
            checked(
                unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) },
                "session start",
            )?;
            let session = Self {
                handle,
                restart_needed: false,
            };
            for executable in no_restart {
                session.filter_executable(executable, RmNoRestart)?;
            }
            for executable in protected {
                session.filter_executable(executable, RmNoShutdown)?;
            }
            // Protect this process last so caller filters cannot override it.
            let current = current_process().context("Unable to protect the current process")?;
            session.filter_process(&current, RmNoShutdown)?;
            session.filter_executable(&std::env::current_exe()?, RmNoShutdown)?;
            // Register in batches: each registration has a registry write cost.
            for batch in files.chunks(256) {
                check_cancelled(cancelled)?;
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

        pub fn affected_processes(&self) -> Result<Vec<u32>> {
            let (affected, reasons) = self.list()?;
            self.check_occupiers(&affected, reasons)?;
            Ok(affected
                .into_iter()
                .map(|process| process.Process.dwProcessId)
                .collect())
        }

        pub fn shutdown(&mut self, force: bool, cancelled: fn() -> bool) -> Result<()> {
            check_cancelled(cancelled)?;
            let (affected, reasons) = self.list()?;
            self.check_occupiers(&affected, reasons)?;
            if !affected.is_empty() {
                self.restart_needed = true;
                checked(self.request_shutdown(force, cancelled), "shutdown")?;
                check_cancelled(cancelled)?;
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
        fn request_shutdown(&self, force: bool, cancelled: fn() -> bool) -> u32 {
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
        pub fn restart(&mut self) -> Result<()> {
            if self.restart_needed {
                self.restart_needed = false;
                checked(unsafe { RmRestart(self.handle, 0, None) }, "restart")?;
            }
            Ok(())
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
    pub fn new(
        _: &[PathBuf],
        _: &[PathBuf],
        _: &[PathBuf],
        cancelled: fn() -> bool,
    ) -> Result<Self> {
        check_cancelled(cancelled)?;
        bail!("File occupancy handling requires Windows Restart Manager")
    }
    pub fn affected_processes(&self) -> Result<Vec<u32>> {
        Ok(vec![])
    }
    pub fn shutdown(&mut self, _: bool, cancelled: fn() -> bool) -> Result<()> {
        check_cancelled(cancelled)
    }
    pub fn restart(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn running() -> bool {
        false
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/restart-manager-tests")
                .join(format!("{:x}", rand::random::<u64>()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn write(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"original").unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn incremental_resources_cover_only_named_files_and_deleted_subtrees() {
        let fixture = Fixture::new();
        fixture.write("python/Lib/untouched/a.pyd");
        let modified = fixture.write("working/main.py");
        let deleted = fixture.write("python/Lib/removed/nested/a.pyd");
        fixture.write("python/Lib/removed/other.pyd");
        let files = existing_files(
            &[
                modified.clone(),
                fixture.0.join("python/Lib/removed"),
                fixture.0.join("absent"),
                modified.clone(),
            ],
            true,
            running,
        )
        .unwrap();
        assert_eq!(files.len(), 3);
        assert!(files.contains(&modified) && files.contains(&deleted));
        assert!(!files
            .iter()
            .any(|path| path.to_string_lossy().contains("untouched")));
        assert!(files.iter().all(|path| path.is_file()));
    }
    #[test]
    fn full_resources_use_entry_points_without_traversing_old_lib() {
        let fixture = Fixture::new();
        let python = fixture.write("python/python.exe");
        let pythonw = fixture.write("python/pythonw.exe");
        fixture.write("python/Lib/untouched/a.pyd");
        let main = fixture.write("working/main.py");
        fixture.write("working/src/old.py");
        let files = existing_files(
            &[fixture.0.join("python"), fixture.0.join("working")],
            false,
            running,
        )
        .unwrap();
        assert_eq!(files.len(), 3);
        assert!(files.contains(&python) && files.contains(&pythonw) && files.contains(&main));
    }
    #[test]
    fn cancellation_stops_resource_collection_before_registering() {
        let fixture = Fixture::new();
        let file = fixture.write("a");
        assert!(existing_files(&[file], true, || true).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn current_launcher_is_protected_and_preflight_fails_before_replacement() {
        let fixture = Fixture::new();
        let path = fixture.write("payload");
        let mut session =
            Session::new(&[std::env::current_exe().unwrap()], &[], &[], running).unwrap();
        assert!(session.shutdown(false, running).is_err());
        assert_eq!(fs::read(path).unwrap(), b"original");
    }

    #[cfg(windows)]
    struct Child(std::process::Child);
    #[cfg(windows)]
    impl Child {
        fn start(fixture: &Fixture, veto: bool) -> Self {
            use std::os::windows::process::CommandExt;
            let exe = fixture.0.join("blocker.exe");
            fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
            let child = std::process::Command::new(exe)
                .args([
                    "--ignored",
                    "--exact",
                    "restart_manager::tests::window_child",
                ])
                .env("PYAPPIFY_RM_READY", fixture.0.join("ready"))
                .env("PYAPPIFY_RM_FILE", fixture.0.join("payload"))
                .env("PYAPPIFY_RM_VETO", if veto { "1" } else { "0" })
                .creation_flags(0x08000000)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let mut child = Self(child);
            for _ in 0..100 {
                if fixture.0.join("ready").exists() {
                    return child;
                }
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "RM fixture exited before opening its window"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            panic!("RM fixture did not become ready");
        }
    }
    #[cfg(windows)]
    impl Drop for Child {
        fn drop(&mut self) {
            // Test-only cleanup of this owned fixture after assertions, never a
            // production fallback for an RM shutdown failure.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "isolated Windows message-window subprocess fixture"]
    fn window_child() {
        use std::{
            os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
            ptr,
        };
        use windows_sys::Win32::{
            Foundation::*, System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*,
        };
        unsafe extern "system" fn procedure(
            window: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            let veto = std::env::var_os("PYAPPIFY_RM_VETO").is_some_and(|value| value == "1");
            match message {
                WM_QUERYENDSESSION => {
                    if veto {
                        0
                    } else {
                        1
                    }
                }
                WM_ENDSESSION if wparam != 0 => {
                    unsafe {
                        PostQuitMessage(0);
                    }
                    0
                }
                WM_CLOSE if !veto => {
                    unsafe {
                        PostQuitMessage(0);
                    }
                    0
                }
                _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
            }
        }
        let Some(ready) = std::env::var_os("PYAPPIFY_RM_READY") else {
            return;
        };
        let file = std::env::var_os("PYAPPIFY_RM_FILE").unwrap();
        let _locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(file)
            .unwrap();
        let class: Vec<u16> = std::ffi::OsStr::new("PyAppifyRMTest")
            .encode_wide()
            .chain(Some(0))
            .collect();
        unsafe {
            // This fixture never needs restarting. Registering this test binary
            // with an empty command line lets RM rerun the entire suite.
            let instance = GetModuleHandleW(ptr::null());
            let definition = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&definition), 0);
            let window = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                1,
                1,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            );
            assert!(!window.is_null());
            fs::write(ready, b"ready").unwrap();
            let mut message = MSG::default();
            while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            DestroyWindow(window);
        }
    }

    #[cfg(windows)]
    #[test]
    fn rm_closes_a_cooperative_external_occupier() {
        let fixture = Fixture::new();
        let file = fixture.write("payload");
        let mut child = Child::start(&fixture, false);
        let mut session = Session::new(
            &[file.clone()],
            &[fixture.0.join("blocker.exe")],
            &[],
            running,
        )
        .unwrap();
        session.shutdown(false, running).unwrap();
        assert!(child.0.try_wait().unwrap().is_some());
        fs::write(&file, b"new").unwrap();
        session.restart().unwrap();
        assert_eq!(fs::read(file).unwrap(), b"new");
    }
    #[cfg(windows)]
    #[test]
    fn rm_veto_does_not_force_kill_or_start_replacement() {
        let fixture = Fixture::new();
        let file = fixture.write("payload");
        let mut child = Child::start(&fixture, true);
        let mut session = Session::new(
            &[file.clone()],
            &[fixture.0.join("blocker.exe")],
            &[],
            running,
        )
        .unwrap();
        assert!(session.shutdown(false, running).is_err());
        assert!(child.0.try_wait().unwrap().is_none());
        assert_eq!(fs::read(file).unwrap(), b"original");
    }

    #[cfg(windows)]
    #[test]
    fn protected_executable_is_rejected_without_shutdown() {
        let fixture = Fixture::new();
        let file = fixture.write("payload");
        let mut child = Child::start(&fixture, false);
        let mut session = Session::new(
            &[file.clone()],
            &[],
            &[fixture.0.join("blocker.exe")],
            running,
        )
        .unwrap();
        assert!(session.shutdown(false, running).is_err());
        assert!(child.0.try_wait().unwrap().is_none());
        assert_eq!(fs::read(file).unwrap(), b"original");
    }
}
