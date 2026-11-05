//! Installation task tracking and source selection for the fork.
use super::cancellation::{self, AppOperationCancellationGuard, OperationKind};
use crate::{
    app_service::{ensure_app_stopped_for_update, get_app_by_name, get_app_lock, setup_git_app},
    mirror::UpdateSource,
    utils::error::Error,
};

#[tauri::command]
pub async fn setup_app(
    app_name: &str,
    profile_name: &str,
    operation_id: Option<String>,
) -> Result<(), Error> {
    let app_dir_lock = get_app_lock(app_name).await?;
    let _guard = app_dir_lock.lock().await;
    let app = get_app_by_name(app_name).await?;
    ensure_app_stopped_for_update(app_name).await?;
    let kind = match (app.update_source, app.installed, app.installed_source()) {
        (UpdateSource::Mirrorchyan, true, UpdateSource::Mirrorchyan) => {
            OperationKind::MirrorConfigure
        }
        (UpdateSource::Mirrorchyan, _, _) => OperationKind::MirrorInstall,
        (_, true, UpdateSource::Git) => OperationKind::GitConfigure,
        _ => OperationKind::GitInstall,
    };
    let mut operation = AppOperationCancellationGuard::start(
        app_name,
        kind,
        operation_id,
        None,
        Some(profile_name.into()),
        app.current_version.clone(),
    );

    let result = if app.update_source == UpdateSource::Mirrorchyan {
        crate::mirror::service::setup(app_name, profile_name, app).await
    } else if app
        .installation
        .as_ref()
        .is_some_and(|record| record.source == UpdateSource::Mirrorchyan)
    {
        crate::source_switch::setup_git_from_mirror(app_name, profile_name, None).await
    } else {
        setup_git_app(app_name, profile_name).await
    };

    let result = result.map_err(|error| {
        if cancellation::is_cancelled(&error) {
            Error::Cancelled
        } else {
            error
        }
    });
    operation.finish(&result);
    result
}
