//! Out-of-process NSIS handoff. The helper is a temporary copy of this binary.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub static SKIP_AUTO_UPDATE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Serialize, Deserialize)]
pub struct Transaction {
    install_dir: PathBuf,
    executable: String,
    app_name: String,
    target: String,
    #[serde(default)]
    previous_version: Option<String>,
    #[serde(default)]
    update_note: Vec<String>,
    parent_pid: u32,
    #[serde(default)]
    helper_pid: Option<u32>,
    update_method: String,
    auto_start: bool,
    current_profile: String,
    state: String,
    error: Option<String>,
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", rand::random::<u64>()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        if unsafe {
            MoveFileExW(
                wide(&temp).as_ptr(),
                wide(path).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            let _ = std::fs::remove_file(&temp);
            return Err(error.into());
        }
    }
    #[cfg(not(windows))]
    std::fs::rename(temp, path)?;
    Ok(())
}

pub fn new_task_dir() -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("pyappify-update-{:016x}", rand::random::<u64>()));
    std::fs::create_dir(&path)?;
    Ok(path)
}

fn pending_path() -> Result<PathBuf> {
    Ok(crate::mirrorchyan::private_dir()?.join("pending.json"))
}
fn write_transaction(dir: &Path, tx: &Transaction) -> Result<()> {
    atomic_write(&dir.join("transaction.json"), &serde_json::to_vec(tx)?)
}

/// Start a helper and wait until it owns a handle to this process. No installer
/// can run until the caller explicitly commits AND this process has exited.
pub async fn prepare(app: &crate::app::App, target: &str, dir: &Path) -> Result<()> {
    let exe = std::env::current_exe()?;
    let tx = Transaction {
        install_dir: crate::utils::path::get_cwd().canonicalize()?,
        executable: exe
            .file_name()
            .context("No executable filename")?
            .to_string_lossy()
            .into(),
        app_name: app.name.clone(),
        target: target.into(),
        previous_version: app.current_version.clone(),
        update_note: app.update_note.clone(),
        parent_pid: std::process::id(),
        helper_pid: None,
        update_method: app.update_method.clone(),
        auto_start: app.auto_start,
        current_profile: app.current_profile.clone(),
        state: "prepared".into(),
        error: None,
    };
    validate_transaction(&tx)?;
    write_transaction(dir, &tx)?;
    let helper = dir.join("update-helper.exe");
    tokio::fs::copy(&exe, &helper).await?;
    let pending = pending_path()?;
    std::fs::create_dir_all(pending.parent().unwrap())?;
    atomic_write(&pending, &serde_json::to_vec(&dir)?)?;
    let mut command = std::process::Command::new(helper);
    command.arg("--installer-helper").arg(dir).current_dir(dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .context("Could not start the installer helper")?;
    for _ in 0..150 {
        if dir.join("ready").exists() {
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            bail!("Installer helper exited before becoming ready");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill();
    bail!("Installer helper did not become ready")
}

pub fn commit(dir: &Path) -> Result<()> {
    atomic_write(&dir.join("commit"), b"ready")
}
pub fn cancel(dir: &Path) {
    let _ = atomic_write(&dir.join("abort"), b"abort");
    if let Ok(pending) = pending_path() {
        let _ = std::fs::remove_file(pending);
    }
}

fn validate_transaction(tx: &Transaction) -> Result<()> {
    for name in [&tx.app_name, &tx.executable] {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains(['/', '\\', ':', '"', '\0', '\r', '\n'])
        {
            bail!("Invalid installer transaction filename");
        }
    }
    if !tx.install_dir.is_absolute()
        || tx.install_dir.parent().is_none()
        || tx
            .install_dir
            .to_string_lossy()
            .contains(['"', '\0', '\r', '\n'])
    {
        bail!("Invalid installation directory");
    }
    Ok(())
}

fn resolve_installed_executable(install_dir: &Path, expected: &str) -> Result<PathBuf> {
    let expected_path = install_dir.join(expected);
    if expected_path.is_file() {
        return Ok(expected_path);
    }

    // Development builds run as `target/debug/pyappify.exe`, while a project
    // setup can install a branded executable such as `ok-nte.exe`. The setup
    // has already completed successfully at this point, so use its sole main
    // executable when the pre-update process name no longer exists.
    let candidates: Vec<_> = std::fs::read_dir(install_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        })
        .filter(|path| {
            !path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("uninstall.exe"))
        })
        .collect();
    match candidates.as_slice() {
        [candidate] => Ok(candidate.clone()),
        [] => bail!(
            "The installer completed, but no application executable was found in {}",
            install_dir.display()
        ),
        _ => bail!(
            "The installer completed, but the application executable could not be identified in {}",
            install_dir.display()
        ),
    }
}

#[cfg(windows)]
fn launch_installed_executable(executable: &Path, install_dir: &Path) -> Result<()> {
    use windows_sys::Win32::UI::{Shell::*, WindowsAndMessaging::SW_SHOWNORMAL};

    let file = wide(executable);
    let cwd = wide(install_dir);
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOASYNC;
        info.lpFile = file.as_ptr();
        info.lpDirectory = cwd.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) == 0 {
            bail!(
                "Windows could not start the updated launcher ({})",
                std::io::Error::last_os_error()
            );
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn launch_installed_executable(executable: &Path, install_dir: &Path) -> Result<()> {
    std::process::Command::new(executable)
        .current_dir(install_dir)
        .spawn()?;
    Ok(())
}

/// Called before normal startup parsing, logging, working-directory changes or
/// single-instance initialization. Helpers never initialize application state.
pub fn try_helper() -> bool {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_none_or(|a| a != "--installer-helper") {
        return false;
    }
    let result = (|| -> Result<()> {
        if args.len() != 3 {
            bail!("Invalid installer helper invocation");
        }
        let dir = PathBuf::from(&args[2]).canonicalize()?;
        if std::env::current_exe()?
            .parent()
            .context("No helper directory")?
            .canonicalize()?
            != dir
        {
            bail!("The installer helper must run from its transaction directory");
        }
        let mut tx: Transaction =
            serde_json::from_slice(&std::fs::read(dir.join("transaction.json"))?)?;
        validate_transaction(&tx)?;
        let result = run_helper(&dir, &mut tx);
        if let Err(error) = &result {
            record_helper_failure(&mut tx, error);
            let _ = write_transaction(&dir, &tx);
        }
        result
    })();
    if let Err(error) = result {
        show_error(&format!("Automatic update could not finish: {error}"));
    }
    true
}

fn record_helper_failure(tx: &mut Transaction, error: &anyhow::Error) {
    // Reopening the launcher happens after a successful installer result was
    // durably recorded. A restart failure must not turn that install into a
    // failed update when the user starts the new launcher manually.
    if tx.state == "succeeded" {
        return;
    }
    tx.state = if tx.state == "prepared" {
        "not_started"
    } else {
        "failed"
    }
    .into();
    tx.error = Some(error.to_string());
}

#[cfg(windows)]
fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn run_helper(dir: &Path, tx: &mut Transaction) -> Result<()> {
    use windows_sys::Win32::{Foundation::WAIT_OBJECT_0, System::Threading::*};
    let parent = ProcessHandle(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, tx.parent_pid) });
    if parent.0.is_null() {
        bail!("Original launcher is no longer available");
    }
    tx.helper_pid = Some(std::process::id());
    write_transaction(dir, tx)?;
    atomic_write(&dir.join("ready"), b"ready")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        if dir.join("abort").exists() {
            return Ok(());
        }
        let exited = unsafe { WaitForSingleObject(parent.0, 100) } == WAIT_OBJECT_0;
        if exited {
            if !dir.join("commit").exists() {
                return Ok(());
            }
            break;
        }
        if std::time::Instant::now() > deadline {
            bail!(
                "Timed out waiting for the original launcher to exit; installation was not started"
            );
        }
    }
    tx.state = "installing".into();
    write_transaction(dir, tx)?;
    let result = launch_installer(&dir.join("setup.exe"), &tx.install_dir);
    match result {
        Ok(0) => {
            tx.state = "succeeded".into();
            tx.error = None;
        }
        Ok(code) => {
            tx.state = "failed".into();
            tx.error = Some(format!(
                "Installer exited with code {code}. Retry using {}",
                dir.join("setup.exe").display()
            ));
        }
        Err(error) => {
            tx.state = "not_started".into();
            tx.error = Some(error.to_string());
        }
    }
    write_transaction(dir, tx)?;
    // No /R: only this helper restarts, after the durable result is available.
    let installed_executable = resolve_installed_executable(&tx.install_dir, &tx.executable)?;
    launch_installed_executable(&installed_executable, &tx.install_dir).with_context(|| {
        format!(
            "Could not reopen the launcher. Run {} to repair the installation.",
            dir.join("setup.exe").display()
        )
    })
}
#[cfg(not(windows))]
fn run_helper(_: &Path, _: &mut Transaction) -> Result<()> {
    bail!("Installer updates require Windows")
}

fn installer_parameters(install_dir: &Path, helper_pid: u32) -> String {
    // NSIS consumes everything after /D=, unquoted; this MUST be the last argument.
    let path = install_dir.to_string_lossy();
    let path = if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        path.strip_prefix("\\\\?\\").unwrap_or(&path).to_string()
    };
    format!("/P /UPDATE /UPDATERPID={helper_pid} /D={path}")
}

#[cfg(windows)]
fn launch_installer(setup: &Path, install_dir: &Path) -> Result<u32> {
    use windows_sys::Win32::{
        System::{Com::*, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::SW_SHOWNORMAL},
    };
    crate::mirrorchyan::validate_pe(setup)?;
    let file = wide(setup);
    let parameters = wide(installer_parameters(install_dir, std::process::id()));
    let cwd = wide(setup.parent().context("Missing installer directory")?);
    unsafe {
        let initialized = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) >= 0;
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        info.lpFile = file.as_ptr();
        info.lpParameters = parameters.as_ptr();
        info.lpDirectory = cwd.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        let success = ShellExecuteExW(&mut info);
        if initialized {
            CoUninitialize();
        }
        if success == 0 || info.hProcess.is_null() {
            bail!(
                "Installer could not start, or elevation was cancelled ({}).",
                std::io::Error::last_os_error()
            );
        }
        let process = ProcessHandle(info.hProcess);
        if WaitForSingleObject(process.0, INFINITE) != windows_sys::Win32::Foundation::WAIT_OBJECT_0
        {
            bail!("Could not wait for installer completion");
        }
        let mut code = 0;
        if GetExitCodeProcess(process.0, &mut code) == 0 {
            bail!("Could not read installer exit status");
        }
        Ok(code)
    }
}

#[cfg(windows)]
fn show_error(message: &str) {
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
            std::ptr::null_mut(),
            wide(message).as_ptr(),
            wide("PyAppify update").as_ptr(),
            0x10,
        );
    }
}
#[cfg(not(windows))]
fn show_error(message: &str) {
    eprintln!("{message}");
}

/// Apply only launcher preferences and the confirmed version, not an old App
/// snapshot: the new installer may have shipped new profiles and metadata.
pub fn consume_result(app: &mut crate::app::App) -> Result<()> {
    let pending = pending_path()?;
    if !pending.exists() {
        return Ok(());
    }
    let receipt = (|| -> Result<(PathBuf, Transaction)> {
        let dir: PathBuf = serde_json::from_slice(&std::fs::read(&pending)?)?;
        let tx: Transaction =
            serde_json::from_slice(&std::fs::read(dir.join("transaction.json"))?)?;
        validate_transaction(&tx)?;
        if tx.install_dir != crate::utils::path::get_cwd().canonicalize()?
            || tx.app_name != app.name
        {
            bail!("Installer result does not match this application");
        }
        Ok((dir, tx))
    })();
    SKIP_AUTO_UPDATE.store(true, Ordering::SeqCst);
    let (dir, tx) = match receipt {
        Ok(receipt) => receipt,
        Err(_) => {
            app.update_source = crate::mirrorchyan::UpdateSource::Mirrorchyan;
            app.update_state = crate::app::AppUpdateState::Failed;
            app.update_phase = Some("install_failed".into());
            app.update_error = Some("The previous install result is missing or unreadable. Check for updates and retry, or run the complete setup to repair.".into());
            return Ok(());
        }
    };
    apply_result(app, tx, &dir);
    // The caller saves App before acknowledging this receipt, so a crash cannot
    // discard the only successful-install record.
    Ok(())
}
fn apply_result(app: &mut crate::app::App, tx: Transaction, dir: &Path) {
    app.update_source = crate::mirrorchyan::UpdateSource::Mirrorchyan;
    app.update_method = tx.update_method;
    app.auto_start = tx.auto_start;
    app.current_profile = tx.current_profile;
    app.update_target_version = Some(tx.target.clone());
    match tx.state.as_str() {
        "succeeded" => {
            app.app_starting_version = tx.previous_version;
            app.update_note = tx.update_note;
            app.current_version = Some(tx.target);
            app.update_state = crate::app::AppUpdateState::Idle;
            app.update_target_version = None;
            app.update_error = None;
            app.update_phase = None;
            app.installed = true;
        }
        "not_started" | "prepared" if tx.state == "not_started" || !dir.join("commit").exists() => {
            app.update_state = crate::app::AppUpdateState::Idle;
            app.update_phase = None;
            app.update_error = Some(tx.error.unwrap_or_else(|| {
                "The previous update stopped before installation. You can retry.".into()
            }));
        }
        _ => {
            app.update_state = crate::app::AppUpdateState::Failed;
            app.update_phase = Some("install_failed".into());
            app.update_error = Some(tx.error.unwrap_or_else(|| {
                format!(
                    "Installation did not finish. Retry using {}",
                    dir.join("setup.exe").display()
                )
            }));
        }
    }
}

pub fn acknowledge_result() {
    if SKIP_AUTO_UPDATE.load(Ordering::SeqCst) {
        if let Ok(path) = pending_path() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Do not consume an unfinished receipt on a second manual launch.
pub fn installation_in_progress() -> bool {
    (|| -> Result<bool> {
        let pending = pending_path()?;
        if !pending.exists() {
            return Ok(false);
        }
        let dir: PathBuf = serde_json::from_slice(&std::fs::read(pending)?)?;
        let tx: Transaction =
            serde_json::from_slice(&std::fs::read(dir.join("transaction.json"))?)?;
        if !matches!(tx.state.as_str(), "prepared" | "installing") || !dir.join("commit").exists() {
            return Ok(false);
        }
        let Some(pid) = tx.helper_pid else {
            return Ok(false);
        };
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(
            sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
            true,
        );
        Ok(sys
            .process(sysinfo::Pid::from_u32(pid))
            .and_then(|p| p.exe())
            .is_some_and(|exe| exe == dir.join("update-helper.exe")))
    })()
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn transaction(dir: &Path, state: &str) -> Transaction {
        Transaction {
            install_dir: dir.to_path_buf(),
            executable: "pyappify.exe".into(),
            app_name: "test".into(),
            target: "v2.0.0".into(),
            previous_version: Some("v1.0.0".into()),
            update_note: vec!["New release".into()],
            parent_pid: std::process::id(),
            helper_pid: None,
            update_method: "MANUAL_UPDATE".into(),
            auto_start: false,
            current_profile: "release".into(),
            state: state.into(),
            error: None,
        }
    }
    fn application() -> crate::app::App {
        serde_yaml::from_str("name: test\ncurrent_version: v1.0.0\nprofiles:\n  - name: release\n    main_script: new_main.py\n").unwrap()
    }
    #[test]
    fn success_receipt_restores_preferences_without_replacing_new_profiles() {
        let dir = new_task_dir().unwrap();
        let mut app = application();
        apply_result(&mut app, transaction(&dir, "succeeded"), &dir);
        assert_eq!(app.current_version.as_deref(), Some("v2.0.0"));
        assert_eq!(app.app_starting_version.as_deref(), Some("v1.0.0"));
        assert_eq!(app.update_note, vec!["New release"]);
        assert_eq!(
            app.update_source,
            crate::mirrorchyan::UpdateSource::Mirrorchyan
        );
        assert_eq!(app.update_method, "MANUAL_UPDATE");
        assert_eq!(app.profiles[0].main_script, "new_main.py");
        assert_eq!(app.update_state, crate::app::AppUpdateState::Idle);
        assert!(app.installed);
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn incomplete_installation_blocks_start_without_advancing_version() {
        let dir = new_task_dir().unwrap();
        for state in ["installing", "failed"] {
            let mut app = application();
            apply_result(&mut app, transaction(&dir, state), &dir);
            assert_eq!(app.current_version.as_deref(), Some("v1.0.0"));
            assert_eq!(app.update_state, crate::app::AppUpdateState::Failed);
            assert_eq!(app.update_phase.as_deref(), Some("install_failed"));
        }
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn cancelled_elevation_and_uncommitted_handoff_leave_old_app_available() {
        let dir = new_task_dir().unwrap();
        for state in ["prepared", "not_started"] {
            let mut app = application();
            apply_result(&mut app, transaction(&dir, state), &dir);
            assert_eq!(app.current_version.as_deref(), Some("v1.0.0"));
            assert_eq!(app.update_state, crate::app::AppUpdateState::Idle);
            assert!(app.update_error.is_some());
        }
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn restart_failure_does_not_replace_successful_install_result() {
        let dir = new_task_dir().unwrap();
        let mut tx = transaction(&dir, "succeeded");
        record_helper_failure(&mut tx, &anyhow::anyhow!("launcher could not restart"));
        assert_eq!(tx.state, "succeeded");
        assert!(tx.error.is_none());
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn transaction_rejects_path_traversal_and_argument_injection() {
        let dir = new_task_dir().unwrap();
        for name in ["../other", "C:\\other", "x\" /S", ".."] {
            let mut tx = transaction(&dir, "prepared");
            tx.executable = name.into();
            assert!(validate_transaction(&tx).is_err());
        }
        std::fs::remove_dir(dir).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn aborted_helper_does_not_launch_installer() {
        let dir = new_task_dir().unwrap();
        let mut tx = transaction(&dir, "prepared");
        atomic_write(&dir.join("abort"), b"abort").unwrap();
        run_helper(&dir, &mut tx).unwrap();
        assert!(dir.join("ready").exists());
        assert_eq!(tx.state, "prepared");
        for name in ["abort", "ready", "transaction.json"] {
            std::fs::remove_file(dir.join(name)).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn nsis_destination_is_last_and_unquoted() {
        assert_eq!(
            installer_parameters(Path::new(r"C:\My App"), 42),
            r"/P /UPDATE /UPDATERPID=42 /D=C:\My App"
        );
    }
    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = new_task_dir().unwrap();
        let path = dir.join("result.json");
        atomic_write(&path, b"old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn branded_installer_executable_is_used_when_development_name_is_absent() {
        let dir = new_task_dir().unwrap();
        std::fs::write(dir.join("ok-nte.exe"), b"test").unwrap();
        std::fs::write(dir.join("uninstall.exe"), b"test").unwrap();
        assert_eq!(
            resolve_installed_executable(&dir, "pyappify.exe").unwrap(),
            dir.join("ok-nte.exe")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
