//! Mirror installation, version refresh and operation status.
use super::UpdateSource;
use crate::app::{save_app_config_to_json, App, AppUpdateState};
use crate::app_service::{emit_app, get_app, is_app_running, APP, STARTUP_OVERRIDES};
use crate::extensions::cancellation::OperationKind;
use crate::{
    emit_error, emit_error_finish, emit_info, emit_success_finish, emitter, err,
    utils::{error::Error, path},
};
use anyhow::{bail, Context, Result};
use std::path::Path;
use sysinfo::{ProcessesToUpdate, System};
use tokio::task;
use tracing::info;

pub(crate) fn installed_mirror_package(
    app: &App,
) -> Result<Option<crate::frozen::package::Package>> {
    if app.installed_source() != UpdateSource::Mirrorchyan {
        return Ok(None);
    }
    let Some((package, _)) = crate::frozen::package::read_package(&path::get_cwd(), &app.name)?
    else {
        return Ok(None);
    };
    if package.app_name != app.name
        || app.mirrorchyan.as_ref().is_none_or(|config| {
            Some(config.resource_id.as_str()) != package.resource_id.as_deref()
        })
    {
        bail!("Mirror package does not match the installed application. Retry installation or update.");
    }
    Ok(Some(package))
}

pub(crate) async fn setup(app_name: &str, profile_name: &str, app: App) -> Result<(), Error> {
    crate::app_service::ensure_app_stopped_for_update(app_name).await?;
    if app.installed && app.installed_source() == UpdateSource::Mirrorchyan {
        let package = installed_mirror_package(&app)?.context("Missing Mirror package")?;
        if !package.profiles.iter().any(|name| name == profile_name)
            || !app
                .profiles
                .iter()
                .any(|profile| profile.name == profile_name)
        {
            return Err(err!(
                "Profile '{}' is unavailable in this frozen package.",
                profile_name
            ));
        }
        crate::extensions::cancellation::begin_commit()?;
        let mut app = app;
        app.current_profile = profile_name.into();
        app.source_operation_state = AppUpdateState::Idle;
        app.source_operation_kind = Some(OperationKind::MirrorConfigure);
        app.source_operation_target = None;
        app.source_operation_error = None;
        app.remember_installation();
        save_app_config_to_json(&app).await?;
        *APP.lock().await = Some(app.configuration());
        emit_app().await;
        emit_success_finish!(app_name);
        return Ok(());
    }
    let api = super::api::Api::new(&app)?;
    api.require_cdk()?;
    let release = match crate::extensions::cancellation::wait(app_name, api.lookup(None))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|value| value)
    {
        Ok(release) => release,
        Err(error) => {
            return finish_failed_operation(app_name, app, None, error, &path::get_cwd()).await
        }
    };
    super::installation::install_zip(
        app_name,
        &release.version_name,
        Some(profile_name),
        api,
        Some(release.clone()),
    )
    .await
}

fn restore_operation_state(
    app: &mut App,
    target: Option<String>,
    cancelled: bool,
    message: String,
) {
    app.source_operation_kind = crate::extensions::cancellation::current_kind(&app.name);
    app.source_operation_state = if cancelled {
        AppUpdateState::Idle
    } else {
        AppUpdateState::Failed
    };
    app.source_operation_target = if cancelled { None } else { target.clone() };
    app.source_operation_error = if cancelled { None } else { Some(message) };
}

/// Retire under the installation lock; leave deletion to the existing background worker.
pub(crate) fn cleanup_installation_backups(root: &Path, app_name: &str) {
    let name = app_name.to_string();
    let operation_id = crate::extensions::cancellation::current_id(app_name);
    match crate::extensions::install_transaction::retire(root) {
        Ok(retired) => {
            task::spawn_blocking(move || {
                if let Err(error) = crate::extensions::file_operations::remove_tree_with_log(
                    &retired,
                    crate::extensions::file_operations::DeletePolicy::Disposable,
                    "Mirror committed backup cleanup",
                    &|| false,
                    Some(crate::extensions::file_operations::LogContext {
                        app_name: name.clone(),
                        operation_id: operation_id.clone(),
                    }),
                ) {
                    emitter::emit_log_for_operation(
                        name,
                        &format!("{error:#}"),
                        false,
                        false,
                        operation_id,
                    );
                }
            });
        }
        Err(error) => {
            emit_info!(
                app_name,
                "Installation succeeded, but backup cleanup is incomplete: {error:#}"
            );
        }
    }
}

pub(crate) async fn finish_failed_operation(
    app_name: &str,
    mut previous: App,
    mut target: Option<String>,
    error: anyhow::Error,
    root: &Path,
) -> Result<(), Error> {
    let recovery_root = root.to_path_buf();
    let recovered = task::spawn_blocking(move || crate::mirror::service::recover(&recovery_root))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
    let cancelled = error
        .downcast_ref::<Error>()
        .is_some_and(crate::extensions::cancellation::is_cancelled)
        && recovered.is_ok();
    if let Some(current) = APP.lock().await.as_ref() {
        previous.auto_start = current.auto_start;
        previous.update_method = current.update_method.clone();
        if target.is_none() {
            target = current.source_operation_target.clone();
        }
    }
    let error_message = match &recovered {
        Ok(_) => format!("{error:#}"),
        Err(rollback) => format!("{error:#}; recovery failed: {rollback:#}"),
    };
    restore_operation_state(&mut previous, target, cancelled, error_message.clone());
    let name = app_name.to_string();
    previous.running = task::spawn_blocking(move || {
        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        is_app_running(&system, &name)
    })
    .await?;
    save_app_config_to_json(&previous).await?;
    *APP.lock().await = Some(previous.configuration());
    emit_app().await;
    crate::mirror::download::progress(
        app_name,
        if cancelled {
            "cancelled"
        } else {
            "install_failed"
        },
        0,
        None,
    );
    if cancelled {
        emitter::emit_cancelled_finish(app_name);
        Err(Error::Cancelled)
    } else {
        emit_error!(app_name, "{}", error_message);
        emit_error_finish!(app_name);
        Err(err!(error_message))
    }
}

pub(crate) async fn refresh() -> Result<bool, Error> {
    refresh_release(None).await
}

/// Give automatic updates a bounded chance before scheduling an installed
/// offline body. The existing ten-second launch delay remains in app_service.
pub(crate) async fn refresh_for_startup() -> Result<bool, Error> {
    refresh_release(Some(std::time::Duration::from_secs(5))).await
}

async fn refresh_release(deadline: Option<std::time::Duration>) -> Result<bool, Error> {
    let stored = APP
        .lock()
        .await
        .clone()
        .ok_or_else(|| err!("App is not loaded."))?;
    if stored.source_operation_state == AppUpdateState::Updating {
        return Ok(false);
    }
    let requested_overrides = STARTUP_OVERRIDES.lock().await.clone();
    let mut app = get_app().await.ok_or_else(|| err!("App is not loaded."))?;

    if app.source_operation_state == AppUpdateState::Updating {
        return Ok(false);
    }
    let started = std::time::Instant::now();
    let release = match deadline {
        Some(deadline) => match tokio::time::timeout(deadline, crate::mirror::api::latest(&app)).await {
            Ok(release) => release,
            Err(_) => Err(anyhow::anyhow!("MirrorChyan startup version check timed out after {} seconds; using the installed application", deadline.as_secs())),
        },
        None => crate::mirror::api::latest(&app).await,
    };
    if deadline.is_some() {
        info!(
            "Mirror startup version check completed in {:.3}s, success={}",
            started.elapsed().as_secs_f64(),
            release.is_ok()
        );
    }
    match release {
        Ok(release) => {
            app.available_versions = if app.source_operation_state == AppUpdateState::Failed
                || crate::mirror::api::newer(&release, app.current_version.as_deref())
            {
                vec![release.version_name]
            } else {
                vec![]
            };
            if app.source_operation_state == AppUpdateState::Idle {
                app.source_operation_error = None;
            }
        }
        Err(error) => {
            app.available_versions.clear();
            app.source_operation_error = Some(error.to_string());
        }
    }
    // Do not overwrite a concurrent installation or a source switch.
    let current_overrides = STARTUP_OVERRIDES.lock().await.clone();
    let mut guard = APP.lock().await;
    if let Some(current) = guard.as_mut().filter(|a| {
        a.update_source == UpdateSource::Mirrorchyan
            && a.source_operation_state == stored.source_operation_state
            && a.current_version == stored.current_version
            && a.source_operation_target == stored.source_operation_target
            && a.update_method == stored.update_method
            && a.mirrorchyan == app.mirrorchyan
            && current_overrides.update_method == requested_overrides.update_method
    }) {
        current.available_versions = app.available_versions;
        current.source_operation_error = app.source_operation_error;
        save_app_config_to_json(current).await?;
    }
    return Ok(true);
}

pub(crate) fn adopt_unpacked(
    app: &mut App,
    package: Option<&crate::frozen::package::Package>,
) -> Result<()> {
    adopt_unpacked_at(app, &path::get_cwd(), package)
}

fn adopt_unpacked_at(
    app: &mut App,
    root: &Path,
    package: Option<&crate::frozen::package::Package>,
) -> Result<()> {
    if let Some(package) = package {
        if package.app_name == app.name
            && package.payload_present(root)?
            && app.mirrorchyan.as_ref().is_some_and(|config| {
                Some(config.resource_id.as_str()) == package.resource_id.as_deref()
            })
        {
            app.installed = true;
            app.current_version = Some(package.version.clone());
            app.update_source = UpdateSource::Mirrorchyan;
            if !package.profiles.contains(&app.current_profile) {
                app.current_profile = package.profiles[0].clone();
            }
            app.commit_installation(UpdateSource::Mirrorchyan);
        }
    }
    Ok(())
}

pub(crate) fn refresh_installed_body(
    app: &mut App,
    package: Option<crate::frozen::package::Package>,
) -> Result<()> {
    let Some(package) = package else {
        app.installed = false;
        return Ok(());
    };
    if package.app_name != app.name
        || app.mirrorchyan.as_ref().is_none_or(|config| {
            Some(config.resource_id.as_str()) != package.resource_id.as_deref()
        })
    {
        bail!("Mirror package does not match the installed application. Retry installation or update.");
    }
    app.installed = package.payload_present(&path::get_cwd())?;
    if app.installed {
        app.current_version = Some(package.version);
        app.remember_installation();
    }
    Ok(())
}

pub(crate) fn restore_interrupted_status(app: &mut App) {
    if app.source_operation_state == AppUpdateState::Updating {
        app.source_operation_state = AppUpdateState::Failed;
        app.source_operation_error =
            Some("The previous operation was interrupted. Retry manually.".into());
    }
}

pub(crate) async fn update_notes(app: &App, version: &str) -> Result<Vec<String>, Error> {
    // A completed release may no longer be the server's latest release. Its notes
    // were saved with the installed payload and do not require another query.
    if app.installed
        && app.installed_source() == UpdateSource::Mirrorchyan
        && app.current_version.as_deref() == Some(version)
    {
        return Ok(app.update_note.clone());
    }
    let release = crate::mirror::api::latest(app).await?;
    if release.version_name != version {
        return Err(err!("Only the latest MirrorChyan release is available"));
    }
    Ok(vec![release.release_note])
}

pub(crate) async fn update(app_name: &str, version: &str, app: App) -> Result<(), Error> {
    crate::app_service::ensure_app_stopped_for_update(app_name).await?;
    let api = super::api::Api::new(&app)?;
    api.require_cdk()?;
    super::installation::install_zip(app_name, version, None, api, None).await
}

pub(crate) fn recover(root: &Path) -> Result<bool> {
    crate::extensions::install_transaction::recover(root)
}

pub(crate) async fn persist_source_operation_state(
    app_name: &str,
    state: AppUpdateState,
    target_version: Option<String>,
    update_error: Option<String>,
) -> Result<()> {
    let app_to_save = {
        let mut app_guard = APP.lock().await;
        let app = app_guard
            .as_mut()
            .filter(|app| app.name == app_name)
            .with_context(|| format!("App '{}' not found.", app_name))?;
        app.source_operation_state = state;
        app.source_operation_kind = crate::extensions::cancellation::current_kind(app_name);
        app.source_operation_target = target_version;
        app.source_operation_error = update_error;
        app.clone()
    };

    save_app_config_to_json(&app_to_save).await?;
    emit_app().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn installed_mirror_notes_are_available_without_remote_configuration() {
        let mut app: App = serde_yaml::from_str("name: example\nprofiles: []\n").unwrap();
        app.installed = true;
        app.current_version = Some("v1.4.11".into());
        app.update_note = vec!["Installed release notes".into()];
        app.commit_installation(UpdateSource::Mirrorchyan);

        assert_eq!(
            update_notes(&app, "v1.4.11").await.unwrap(),
            ["Installed release notes"]
        );
        app.update_note.clear();
        assert!(update_notes(&app, "v1.4.11").await.unwrap().is_empty());
    }

    #[test]
    fn mirror_adoption_uses_intact_local_body_without_root_yaml_or_cdk() {
        let temporary = std::env::temp_dir();
        let root = temporary.join(format!(
            "pyappify-adoption-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        assert_eq!(root.parent(), Some(temporary.as_path()));
        let yaml = "name: example\nauto_start: true\nupdate_method: MANUAL_UPDATE\nmirrorchyan:\n  resource_id: example-resource\nprofiles:\n  - name: default\n    main_script: main.py\n";
        let template = crate::app::parse_app_template(yaml).unwrap();
        let package = crate::frozen::package::Package {
            app_name: "example".into(),
            version: "v1.4.9".into(),
            resource_id: Some("example-resource".into()),
            executable: "application.exe".into(),
            profiles: vec!["default".into()],
        };
        let working = root.join(package.base()).join("working");
        std::fs::create_dir_all(working.join("_internal")).unwrap();
        std::fs::write(working.join("pyappify.yml"), yaml).unwrap();
        std::fs::write(working.join("application.exe"), b"fixture").unwrap();
        std::fs::write(working.join("_internal/python312.dll"), b"fixture").unwrap();
        crate::frozen::program_files::ProgramFiles::capture_clean_body(
            &working,
            "v1.4.9",
            "application.exe",
        )
        .unwrap();
        assert!(!root.join("pyappify.yml").exists());
        let mut app = template.clone();
        app.installed = true;
        app.current_version = Some("v1.4.8".into());
        app.commit_installation(UpdateSource::Mirrorchyan);
        let (body, working_config) = crate::frozen::package::read_package(&root, &app.name)
            .unwrap()
            .unwrap();
        app = crate::app::merge_working_template(app, working_config).unwrap();
        adopt_unpacked_at(&mut app, &root, Some(&body)).unwrap();
        assert!(app.installed && app.auto_start);
        assert_eq!(app.name, template.name);
        assert_eq!(app.current_version.as_deref(), Some("v1.4.9"));
        assert_eq!(app.installed_source(), UpdateSource::Mirrorchyan);
        assert_eq!(app.update_method, "MANUAL_UPDATE");
        std::fs::remove_file(working.join("application.exe")).unwrap();
        let mut incomplete = template;
        adopt_unpacked_at(&mut incomplete, &root, Some(&body)).unwrap();
        assert!(!incomplete.installed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_operation_rollback_does_not_change_git_retry_or_committed_body() {
        let mut app: App = serde_yaml::from_str("name: example\nprofiles: []\n").unwrap();
        app.installed = true;
        app.current_version = Some("v1.4.8".into());
        app.update_state = AppUpdateState::Failed;
        app.update_target_version = Some("v1.4.9".into());
        app.update_error = Some("pip failed".into());
        app.remember_installation();
        let previous = app.clone();
        restore_operation_state(&mut app, Some("v2.0.0".into()), true, "cancelled".into());
        assert_eq!(app.update_state, previous.update_state);
        assert_eq!(app.update_target_version, previous.update_target_version);
        assert_eq!(app.update_error, previous.update_error);
        assert_eq!(app.installation, previous.installation);
        assert_eq!(app.source_operation_state, AppUpdateState::Idle);
        assert!(app.source_operation_target.is_none());
        restore_operation_state(
            &mut app,
            Some("v2.0.0".into()),
            false,
            "unpack failed".into(),
        );
        assert_eq!(app.source_operation_state, AppUpdateState::Failed);
        assert_eq!(app.update_target_version, previous.update_target_version);
        assert_eq!(app.installation, previous.installation);
    }
}
