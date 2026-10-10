//! Wrap the original Git installer only when replacing a Mirror installation.
use crate::extensions::{
    file_operations::safe_join,
    install_transaction::{Action, InstallTransaction, TASK_DIR},
};
use crate::frozen::program_files::{self, ProgramFiles};
use crate::{
    app::{load_app_config_from_json, AppUpdateState, YML_FILE_NAME},
    app_service::{
        emit_app, ensure_app_stopped_for_update, get_app_by_name, load_app_details,
        resolve_current_version_state, setup_git_files_from_repository, APP,
    },
    emit_success_finish,
    extensions::cancellation::app_operation_cancelled,
    git,
    mirror::service::{cleanup_installation_backups, finish_failed_operation},
    utils::{error::Error, path},
};
use anyhow::{bail, Context, Result};
use std::{fs, path::Path};
use tokio::task;

/// Stop the app before calling this. Installers use these final paths directly.
pub(crate) fn begin(
    root: &Path,
    app_name: &str,
    rollback_config: &str,
    cancelled: impl Fn() -> bool + Sync,
) -> Result<InstallTransaction> {
    if safe_join(root, TASK_DIR)?.exists() {
        bail!("Recover the previous installation before installing");
    }
    let mut transaction = InstallTransaction::new(root, app_name)?;
    fs::create_dir_all(root.join(TASK_DIR).join("staged"))?;
    let base = transaction.base();
    let repo = format!("{base}/repo");
    let backup = transaction.backup(&repo)?;
    transaction
        .actions
        .push(Action::OwnedTree { path: repo, backup });
    transaction.apply_preparation(rollback_config, cancelled)?;
    Ok(transaction)
}

fn prepare_body(
    transaction: &mut InstallTransaction,
    rollback_config: &str,
) -> Result<Option<String>> {
    let base = transaction.base();
    let working = format!("{base}/working");
    let backup = transaction.backup(&working)?;
    transaction.actions.push(Action::OwnedTree {
        path: working,
        backup: backup.clone(),
    });
    let python = format!("{base}/python");
    let python_backup = transaction.backup(&python)?;
    transaction.actions.push(Action::OwnedTree {
        path: python,
        backup: python_backup,
    });
    transaction.apply_preparation(rollback_config, app_operation_cancelled)?;
    Ok(backup)
}

pub(crate) fn prepare_commit(
    transaction: &mut InstallTransaction,
    new_program: ProgramFiles,
    user_backup: Option<(String, ProgramFiles)>,
) -> Result<()> {
    let working = transaction.root.join(transaction.base()).join("working");
    if !transaction
        .root
        .join(transaction.base())
        .join("python/python.exe")
        .is_file()
        || !working.join(YML_FILE_NAME).is_file()
    {
        bail!("Incomplete Git installation");
    }
    for file in &new_program.files {
        if app_operation_cancelled() {
            return Err(crate::utils::error::Error::Cancelled.into());
        }
        let path = working.join(file);
        program_files::reject_link(&path)?;
        if !path.is_file() {
            bail!("Incomplete Git installation: missing program file {file}");
        }
    }
    if let Some((backup, old)) = user_backup {
        program_files::migrate_users(
            &safe_join(&transaction.root, &backup)?,
            &working,
            &old,
            &new_program,
            app_operation_cancelled,
        )?;
    }
    Ok(())
}

pub(crate) async fn setup_git_from_mirror(
    app_name: &str,
    profile_name: &str,
    version: Option<&str>,
) -> Result<(), Error> {
    ensure_app_stopped_for_update(app_name).await?;
    let root = path::get_cwd();
    let recovery_root = root.clone();
    if task::spawn_blocking(move || crate::mirror::service::recover(&recovery_root)).await?? {
        if let Some(restored) = load_app_config_from_json(app_name).await? {
            *APP.lock().await = Some(restored);
        }
    }
    let previous = get_app_by_name(app_name).await?;
    let old_program = installed_program(&root, &previous)?;
    crate::mirror::service::persist_source_operation_state(
        app_name,
        AppUpdateState::Updating,
        version.map(str::to_string),
        None,
    )
    .await?;
    let result: Result<()> = async {
        let (install_root, name, rollback) = (
            root.clone(),
            app_name.to_string(),
            serde_json::to_string_pretty(&previous.configuration())?,
        );
        let mut transaction = task::spawn_blocking(move || {
            begin(&install_root, &name, &rollback, app_operation_cancelled)
        })
        .await??;
        crate::extensions::cancellation::ensure_app_operation_not_cancelled()?;
        let mut clone_app = previous.clone();
        clone_app.current_profile = profile_name.into();
        git::ensure_repository(&clone_app).await?;
        crate::extensions::cancellation::ensure_app_operation_not_cancelled()?;
        if let Some(version) = version {
            git::checkout_version_tag(app_name, &path::get_app_repo_path(app_name), version)
                .await?;
        }
        crate::extensions::cancellation::ensure_app_operation_not_cancelled()?;
        let rollback = serde_json::to_string_pretty(&previous.configuration())?;
        let (prepared, working_backup) = task::spawn_blocking(move || -> Result<_> {
            let backup = prepare_body(&mut transaction, &rollback)?;
            Ok((transaction, backup))
        })
        .await??;
        transaction = prepared;
        let final_profile = setup_git_files_from_repository(app_name, profile_name).await?;
        crate::extensions::cancellation::ensure_app_operation_not_cancelled()?;
        let repo = path::get_app_repo_path(app_name);
        let (versions, current) = git::get_tags_and_current_version(app_name, repo.clone()).await?;
        let mut updated = previous.clone();
        load_app_details(&mut updated).await?;
        updated.current_profile = final_profile;
        (updated.current_version, updated.current_version_missing) =
            resolve_current_version_state(None, &versions, current);
        updated.available_versions = versions;
        updated.installed = true;
        updated.source_operation_kind = crate::extensions::cancellation::current_kind(app_name);
        updated.source_operation_state = AppUpdateState::Idle;
        updated.source_operation_target = None;
        updated.source_operation_error = None;
        updated.app_starting_version = previous
            .installation
            .as_ref()
            .and_then(|record| record.version.clone());
        updated.commit_installation(crate::mirror::UpdateSource::Git);
        updated.update_state = AppUpdateState::Idle;
        updated.update_target_version = None;
        updated.update_error = None;
        let preferences = get_app_by_name(app_name).await?;
        updated.auto_start = preferences.auto_start;
        updated.update_method = preferences.update_method.clone();
        let program = crate::frozen::program_files::ProgramFiles::git(&repo, None)?;
        prepare_commit(&mut transaction, program, working_backup.zip(old_program))?;
        let mut rollback_app = previous.clone();
        rollback_app.auto_start = preferences.auto_start;
        rollback_app.update_method = preferences.update_method;
        let config = serde_json::to_vec_pretty(&updated.configuration())?;
        let rollback_config = serde_json::to_string_pretty(&rollback_app.configuration())?;
        crate::extensions::cancellation::begin_commit()?;
        task::spawn_blocking(move || {
            transaction.apply(&config, &rollback_config, app_operation_cancelled)
        })
        .await??;
        *APP.lock().await = Some(updated.configuration());
        emit_app().await;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            cleanup_installation_backups(&root, app_name);
            emit_success_finish!(app_name);
            Ok(())
        }
        Err(error) => {
            finish_failed_operation(
                app_name,
                previous,
                version.map(str::to_string),
                error,
                &root,
            )
            .await
        }
    }
}

/// Read the committed route/version, not the currently selected source.
pub(crate) fn installed_program(
    root: &Path,
    app: &crate::app::App,
) -> Result<Option<ProgramFiles>> {
    let base = safe_join(root, &format!("data/apps/{}", app.name))?;
    let working = safe_join(&base, "working")?;
    match fs::metadata(&working) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => bail!(
            "Unexpected application body path type: {}",
            working.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to inspect {}", working.display()))
        }
    }
    if fs::read_dir(&working)?.next().transpose()?.is_none() {
        return Ok(None);
    }
    if app.installed_source() == crate::mirror::UpdateSource::Git {
        return Ok(Some(ProgramFiles::git(
            &base.join("repo"),
            app.current_version.as_deref(),
        )?));
    }
    Ok(Some(ProgramFiles::read(&working)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_preparation_keeps_the_body_until_setup_and_recovery_restores_it() {
        let root = std::env::temp_dir().join(format!(
            "pyappify-source-transaction-{:x}",
            rand::random::<u64>()
        ));
        let base = root.join("data/apps/example");
        for (name, file) in [
            ("working", "user"),
            ("python", "python.exe"),
            ("repo", "old-repo"),
        ] {
            fs::create_dir_all(base.join(name)).unwrap();
            fs::write(base.join(name).join(file), b"original").unwrap();
        }
        let mut transaction = begin(&root, "example", "{}", || false).unwrap();
        assert_eq!(fs::read(base.join("working/user")).unwrap(), b"original");
        assert_eq!(
            fs::read(base.join("python/python.exe")).unwrap(),
            b"original"
        );
        fs::create_dir_all(base.join("repo")).unwrap();
        fs::write(base.join("repo/new-repo"), b"new").unwrap();
        prepare_body(&mut transaction, "{}").unwrap();
        fs::create_dir_all(base.join("working")).unwrap();
        fs::write(base.join("working/new-body"), b"new").unwrap();
        crate::extensions::install_transaction::recover(&root).unwrap();
        assert_eq!(fs::read(base.join("working/user")).unwrap(), b"original");
        assert_eq!(
            fs::read(base.join("python/python.exe")).unwrap(),
            b"original"
        );
        assert_eq!(fs::read(base.join("repo/old-repo")).unwrap(), b"original");
        assert!(!base.join("working/new-body").exists());
        assert!(!base.join("repo/new-repo").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
