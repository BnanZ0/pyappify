//! Mirror ZIP operations: one full installation route and a separate delta plan.
use super::{
    api::{self, Release},
    archive, service, UpdateSource,
};
use crate::frozen::{package, program_files};
use crate::{
    app::{load_app_config_from_json, merge_working_template, App, AppUpdateState},
    app_service::{emit_app, get_app_by_name, APP},
    emit_info, emit_success_finish,
    extensions::cancellation::{app_operation_cancelled, ensure_app_operation_not_cancelled},
    utils::{error::Error, path},
};
use anyhow::{bail, Result};
use std::{fs, path::PathBuf, time::Instant};
use tokio::task;
use tracing::info;

struct ZipInstallation {
    app_name: String,
    version: String,
    profile_name: Option<String>,
    root: PathBuf,
    resource_id: String,
    previous: App,
    zip_input: super::download::ZipInput,
    api: api::Api,
    initial_release: Option<Release>,
    started: Instant,
}

impl ZipInstallation {
    async fn check_active(&self) -> Result<()> {
        ensure_app_operation_not_cancelled()?;
        let current = get_app_by_name(&self.app_name).await?;
        if current.update_source != UpdateSource::Mirrorchyan
            || current.source_operation_state != AppUpdateState::Updating
            || current.source_operation_target.as_deref() != Some(self.version.as_str())
        {
            bail!("The active update changed before file replacement");
        }
        Ok(())
    }
}

pub(super) async fn install_zip(
    app_name: &str,
    version: &str,
    profile_name: Option<&str>,
    api: api::Api,
    initial_release: Option<Release>,
) -> Result<(), Error> {
    let root = path::get_cwd();
    let local_input = super::download::local_zip_input(&root)?;
    let recovery_root = root.clone();
    if task::spawn_blocking(move || service::recover(&recovery_root)).await?? {
        if let Some(restored) = load_app_config_from_json(app_name).await? {
            *APP.lock().await = Some(restored);
            emit_app().await;
        }
    }
    let previous = get_app_by_name(app_name).await?;
    if previous.update_source != UpdateSource::Mirrorchyan
        || previous.source_operation_state == AppUpdateState::Updating
    {
        return Err(crate::err!(
            "Another update is in progress or the update source changed"
        ));
    }
    let old_program = crate::source_switch::installed_program(&root, &previous)?;
    let resource_id = api.resource_id.clone();
    if crate::git::compare_version_tags(version, version).is_none() {
        return Err(crate::err!("Invalid MirrorChyan version"));
    }
    let current_version =
        if previous.installed && previous.installed_source() == UpdateSource::Mirrorchyan {
            old_program.as_ref().map(|program| program.version.clone())
        } else {
            None
        };
    let zip_input = match local_input {
        Some(input) => input,
        None => {
            let download_path = super::download::download_dir()
                .join(format!("zip-{:x}.download", rand::random::<u64>()));
            fs::create_dir_all(download_path.parent().unwrap())?;
            super::download::ZipInput::Temporary(download_path)
        }
    };
    super::service::persist_source_operation_state(
        app_name,
        AppUpdateState::Updating,
        Some(version.into()),
        None,
    )
    .await?;
    let request = ZipInstallation {
        app_name: app_name.into(),
        version: version.into(),
        profile_name: profile_name.map(str::to_string),
        root,
        resource_id,
        previous,
        zip_input,
        api,
        initial_release,
        started: Instant::now(),
    };
    let result: Result<()> = async {
        if let Some(current_version) = current_version.filter(|current| current != version) {
            super::download::progress(app_name, "downloading", 0, None);
            let release = request
                .zip_input
                .acquire(
                    &request.previous,
                    version,
                    Some(&current_version),
                    &request.api,
                    None,
                )
                .await?;
            let archive_path = request.zip_input.path().to_path_buf();
            let incremental =
                task::spawn_blocking(move || archive::is_incremental(&archive_path)).await??;
            if !incremental {
                // The server chose a full response for a baseline request. Reuse
                // those bytes; no second download or second backup is needed.
                return install_full(&request, old_program, Some(release)).await;
            }
            let (archive_path, install_root, name, rid, target) = (
                request.zip_input.path().to_path_buf(),
                request.root.clone(),
                request.app_name.clone(),
                request.resource_id.clone(),
                request.version.clone(),
            );
            super::download::progress(app_name, "extracting", 0, None);
            let prepared = task::spawn_blocking(move || {
                archive::prepare_incremental(
                    &archive_path,
                    &install_root,
                    &package::Expected {
                        app_name: &name,
                        resource_id: &rid,
                        version: &target,
                    },
                    app_operation_cancelled,
                )
            })
            .await?;
            match prepared {
                Ok(prepared) => return commit_zip(&request, prepared, release).await,
                Err(error) if error.is::<archive::NeedsFullPackage>() => {
                    emit_info!(
                        app_name,
                        "The ZIP baseline no longer matches. Downloading a complete package."
                    );
                }
                Err(error) => return Err(error),
            }
        }
        install_full(&request, old_program, None).await
    }
    .await;
    if let Some(temporary) = request.zip_input.temporary_path() {
        let temporary = temporary.to_path_buf();
        let name = app_name.to_string();
        let operation_id = crate::extensions::cancellation::current_id(app_name);
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(error) = crate::extensions::file_operations::remove_file_with_log(
                &temporary,
                "Mirror downloaded ZIP cleanup",
                &|| false,
                Some(crate::extensions::file_operations::LogContext {
                    app_name: name.clone(),
                    operation_id: operation_id.clone(),
                }),
            ) {
                crate::emitter::emit_log_for_operation(
                    name,
                    &format!(
                        "Unable to clean downloaded ZIP {}: {error:#}",
                        temporary.display()
                    ),
                    false,
                    false,
                    operation_id,
                );
            }
        });
    }
    match result {
        Ok(()) => {
            service::cleanup_installation_backups(&request.root, app_name);
            super::download::progress(app_name, "completed", 0, None);
            emit_success_finish!(app_name);
            Ok(())
        }
        Err(error) => {
            service::finish_failed_operation(
                app_name,
                request.previous,
                Some(version.into()),
                error,
                &request.root,
            )
            .await
        }
    }
}

/// Fresh installs, source switches, reinstalls and delta fallback all come here.
async fn install_full(
    request: &ZipInstallation,
    old_program: Option<program_files::ProgramFiles>,
    downloaded: Option<Release>,
) -> Result<()> {
    request.check_active().await?;
    super::download::progress(&request.app_name, "preparing", 0, None);
    let (root, name, rollback) = (
        request.root.clone(),
        request.app_name.clone(),
        serde_json::to_string_pretty(&request.previous)?,
    );
    let (transaction, user_backup) = task::spawn_blocking(move || {
        let (mut transaction, backup) = archive::plan_installation(&root, &name)?;
        transaction.apply_preparation(&rollback, app_operation_cancelled)?;
        Ok::<_, anyhow::Error>((transaction, backup.zip(old_program)))
    })
    .await??;
    emit_info!(
        &request.app_name,
        "Existing working directory was backed up for the complete ZIP."
    );
    let release = match downloaded {
        Some(release) => release,
        None => {
            super::download::progress(&request.app_name, "downloading", 0, None);
            request
                .zip_input
                .acquire(
                    &request.previous,
                    &request.version,
                    None,
                    &request.api,
                    request.initial_release.as_ref(),
                )
                .await?
        }
    };
    super::download::progress(&request.app_name, "extracting", 0, None);
    let (archive_path, root, name, rid, target) = (
        request.zip_input.path().to_path_buf(),
        request.root.clone(),
        request.app_name.clone(),
        request.resource_id.clone(),
        request.version.clone(),
    );
    let prepared = task::spawn_blocking(move || {
        archive::stage_full(
            &archive_path,
            &root,
            &package::Expected {
                app_name: &name,
                resource_id: &rid,
                version: &target,
            },
            transaction,
            user_backup,
            app_operation_cancelled,
        )
    })
    .await??;
    commit_zip(request, prepared, release).await
}

async fn commit_zip(
    request: &ZipInstallation,
    prepared: archive::Prepared,
    release: Release,
) -> Result<()> {
    let app_name = request.app_name.as_str();
    let version = request.version.as_str();
    let profile_name = request.profile_name.as_deref();
    let previous = &request.previous;
    info!(
        "ZIP validation {:.3}s, extraction {:.3}s, incremental={}",
        prepared.validation_seconds, prepared.extraction_seconds, prepared.incremental
    );
    ensure_app_operation_not_cancelled()?;
    let mut updated = get_app_by_name(app_name).await?;
    updated = merge_working_template(updated, prepared.working_config.clone())?;
    if let Some(profile) = profile_name {
        updated.current_profile = profile.into();
    }
    if updated.current_profile.is_empty() {
        updated.current_profile = prepared.package.profiles[0].clone();
    }
    if !prepared.package.profiles.contains(&updated.current_profile)
        || !updated
            .profiles
            .iter()
            .any(|profile| profile.name == updated.current_profile)
    {
        bail!(
            "Profile '{}' is unavailable in this frozen package.",
            updated.current_profile
        );
    }
    updated.installed = true;
    updated.running = false;
    updated.current_version = Some(version.into());
    updated.current_version_missing = false;
    updated.app_starting_version = previous.current_version.clone().or_else(|| {
        previous
            .installation
            .as_ref()
            .and_then(|installed| installed.version.clone())
    });
    updated.available_versions.clear();
    updated.update_note = if release.release_note.is_empty() {
        vec![]
    } else {
        vec![release.release_note]
    };
    updated.source_operation_kind = crate::extensions::cancellation::current_kind(app_name);
    updated.source_operation_state = AppUpdateState::Idle;
    updated.source_operation_target = None;
    updated.source_operation_error = None;
    updated.commit_installation(UpdateSource::Mirrorchyan);
    updated.update_state = AppUpdateState::Idle;
    updated.update_target_version = None;
    updated.update_error = None;
    let preferences = get_app_by_name(app_name).await?;
    updated.auto_start = preferences.auto_start;
    updated.update_method = preferences.update_method.clone();
    let mut rollback_app = previous.clone();
    rollback_app.auto_start = preferences.auto_start;
    rollback_app.update_method = preferences.update_method;
    let config = serde_json::to_vec_pretty(&updated.configuration())?;
    let rollback_config = serde_json::to_string_pretty(&rollback_app.configuration())?;
    request.check_active().await?;
    super::download::progress(app_name, "installing", 0, None);
    let apply_started = Instant::now();
    crate::extensions::cancellation::begin_commit()?;
    task::spawn_blocking(move || {
        prepared.apply(&config, &rollback_config, app_operation_cancelled)
    })
    .await??;
    *APP.lock().await = Some(updated.configuration());
    emit_app().await;
    emit_info!(
        app_name,
        "ZIP file application completed in {:.3} s; total update {:.3} s.",
        apply_started.elapsed().as_secs_f64(),
        request.started.elapsed().as_secs_f64()
    );
    Ok(())
}
