use crate::emit_info;
use anyhow::{Context, Result};
use git2::Repository;

pub fn update_repository_submodules(
    repo: &Repository,
    app_name: &str,
    context_message: &str,
) -> Result<()> {
    let submodules = repo
        .submodules()
        .with_context(|| format!("Failed to load submodules for {}", context_message))?;

    if !submodules.is_empty() {
        emit_info!(
            app_name,
            "Found {} submodules for {}. Updating them...",
            submodules.len(),
            context_message
        );
        for mut submodule in submodules {
            let token = crate::extensions::cancellation::current_token(app_name);
            let update = if let Some(token) = token {
                token.check()?;
                let mut callbacks = git2::RemoteCallbacks::new();
                callbacks.transfer_progress(move |_| token.git_progress());
                let mut fetch = git2::FetchOptions::new();
                fetch.remote_callbacks(callbacks);
                let mut options = git2::SubmoduleUpdateOptions::new();
                options.fetch(fetch);
                submodule.update(true, Some(&mut options))
            } else {
                submodule.update(true, None)
            };
            crate::extensions::cancellation::git_result(
                app_name,
                update.with_context(|| {
                    format!(
                        "Failed to update submodule '{}' for {}",
                        submodule.name().unwrap_or("<unknown>"),
                        context_message
                    )
                }),
            )?;
            emit_info!(
                app_name,
                "Successfully updated submodule: {} for {}",
                submodule.name().unwrap_or("<unknown>"),
                context_message
            );
        }
        emit_info!(app_name, "All submodules updated for {}.", context_message);
    } else {
        emit_info!(
            app_name,
            "No submodules found to update for {}.",
            context_message
        );
    }
    Ok(())
}
