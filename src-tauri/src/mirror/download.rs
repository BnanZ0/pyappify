//! Cancellable ZIP downloads with progress and optional SHA-256 verification.
use super::api::{Api, Release};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
#[cfg(debug_assertions)]
use std::fs;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

/// Keep temporary downloads in the installation's cache, outside replaced app data.
pub(super) fn download_dir() -> PathBuf {
    crate::utils::path::get_cwd()
        .join("cache")
        .join("mirrorchyan")
}

/// Only temporary downloads belong to the operation and may be removed.
pub(super) enum ZipInput {
    Temporary(PathBuf),
    #[cfg(debug_assertions)]
    Local {
        path: PathBuf,
        release: Release,
    },
}

impl ZipInput {
    pub(super) fn path(&self) -> &Path {
        match self {
            Self::Temporary(path) => path,
            #[cfg(debug_assertions)]
            Self::Local { path, .. } => path,
        }
    }

    pub(super) fn temporary_path(&self) -> Option<&Path> {
        match self {
            Self::Temporary(path) => Some(path),
            #[cfg(debug_assertions)]
            Self::Local { .. } => None,
        }
    }

    pub(super) async fn acquire(
        &self,
        app: &crate::app::App,
        target: &str,
        baseline: Option<&str>,
        api: &Api,
        release: Option<&Release>,
    ) -> Result<Release> {
        match self {
            #[cfg(debug_assertions)]
            Self::Local { path, release } => {
                crate::extensions::cancellation::ensure_app_operation_not_cancelled()?;
                if release.version_name != target {
                    bail!("The local ZIP version changed");
                }
                let total = tokio::fs::metadata(path).await?.len();
                // Archive preparation opens this file read-only and keeps every
                // existing validation; no copy or write is needed here.
                progress(&app.name, "downloading", 0, Some(total));
                progress(&app.name, "downloaded", total, Some(total));
                Ok(release.clone())
            }
            Self::Temporary(path) => download(app, target, baseline, path, api, release).await,
        }
    }
}

/// Capture and check the borrowed input before recovery or backup can move it.
pub(super) fn local_zip_input(root: &Path) -> Result<Option<ZipInput>> {
    #[cfg(debug_assertions)]
    {
        let Some((path, release)) = local_zip_release()? else {
            return Ok(None);
        };
        let path = fs::canonicalize(path).context("Unable to resolve the local ZIP input")?;
        let root =
            fs::canonicalize(root).context("Unable to resolve the installation directory")?;
        validate_local_zip_location(&path, &root)?;
        // Check readability before displacing the installed application.
        fs::File::open(&path).context("Unable to read the local ZIP input")?;
        Ok(Some(ZipInput::Local { path, release }))
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = root;
        Ok(None)
    }
}

#[cfg(debug_assertions)]
fn validate_local_zip_location(path: &Path, root: &Path) -> Result<()> {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase()
    };
    let root = normalize(root) + "/";
    let path = normalize(path);
    let Some(relative) = path.strip_prefix(&root) else {
        return Ok(());
    };
    let tree = relative.split('/').next().unwrap_or_default();
    if crate::frozen::package::within(relative, "data/apps")
        || tree == crate::extensions::install_transaction::TASK_DIR
        || tree.starts_with(&format!(
            "{}-completed-",
            crate::extensions::install_transaction::TASK_DIR
        ))
    {
        bail!("Keep the local ZIP outside installation data, transaction directories and replaced metadata; those paths can be moved or cleaned up");
    }
    Ok(())
}

#[derive(Clone, Serialize)]
pub struct Progress<'a> {
    pub operation_id: Option<String>,
    pub app_name: &'a str,
    pub phase: &'a str,
    pub downloaded: u64,
    pub total: Option<u64>,
}
pub fn progress(app: &str, phase: &str, downloaded: u64, total: Option<u64>) {
    crate::emitter::emit(
        "mirror-update-progress",
        Progress {
            operation_id: crate::extensions::cancellation::current_id(app),
            app_name: app,
            phase,
            downloaded,
            total,
        },
    );
}

/// No URL or CDK is included in download errors or filenames.
pub(super) async fn download(
    app: &crate::app::App,
    target: &str,
    baseline: Option<&str>,
    destination: &Path,
    api: &Api,
    known_release: Option<&Release>,
) -> Result<Release> {
    for attempt in 0..2 {
        let release = if attempt == 0 {
            match known_release {
                Some(release) => release.clone(),
                None => {
                    crate::extensions::cancellation::wait(&app.name, api.lookup(baseline)).await??
                }
            }
        } else {
            crate::extensions::cancellation::wait(&app.name, api.lookup(baseline)).await??
        };
        if release.version_name != target {
            bail!("The latest release changed. Check for updates again.");
        }
        let url = release
            .url
            .as_deref()
            .context("No ZIP download available. Configure a valid MirrorChyan CDK.")?;
        let parsed =
            reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("Invalid ZIP download URL"))?;
        if parsed.scheme() != "https" {
            bail!("ZIP download requires HTTPS");
        }
        let response =
            crate::extensions::cancellation::wait(&app.name, api.client.get(parsed).send())
                .await?
                .map_err(|_| {
                    anyhow::anyhow!("ZIP download failed; check your connection and retry")
                })?;
        if attempt == 0 && matches!(response.status().as_u16(), 401 | 403 | 410) {
            continue;
        }
        if !response.status().is_success() {
            bail!("ZIP download failed (HTTP {})", response.status().as_u16());
        }
        receive_zip(&app.name, response, release.sha256.as_deref(), destination).await?;
        return Ok(release);
    }
    bail!("ZIP download URL expired; retry the update")
}

async fn receive_zip(
    app_name: &str,
    response: reqwest::Response,
    expected: Option<&str>,
    destination: &Path,
) -> Result<()> {
    if expected.is_some_and(|hash| hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        bail!("Invalid ZIP checksum metadata");
    }
    let total = response.content_length();
    progress(app_name, "downloading", 0, total);
    let mut output = tokio::fs::File::create(destination).await?;
    let mut stream = response.bytes_stream();
    let mut digest = expected.map(|_| Sha256::new());
    let mut size = 0u64;
    let mut last_emit = std::time::Instant::now();
    while let Some(chunk) = crate::extensions::cancellation::wait(app_name, stream.next()).await? {
        let chunk =
            chunk.map_err(|_| anyhow::anyhow!("ZIP download interrupted; retry the update"))?;
        output.write_all(&chunk).await?;
        if let Some(digest) = &mut digest {
            digest.update(&chunk);
        }
        size += chunk.len() as u64;
        if last_emit.elapsed() >= Duration::from_millis(200) {
            progress(app_name, "downloading", size, total);
            last_emit = std::time::Instant::now();
        }
    }
    output.sync_all().await?;
    if size == 0 || total.is_some_and(|n| n != size) {
        bail!("Incomplete ZIP download");
    }
    if expected
        .is_some_and(|hash| !format!("{:x}", digest.unwrap().finalize()).eq_ignore_ascii_case(hash))
    {
        bail!("ZIP SHA-256 checksum mismatch");
    }
    progress(app_name, "downloaded", size, total);
    Ok(())
}

/// Local ZIP tests use the same preparation/apply path as remote updates.
#[cfg(debug_assertions)]
pub(super) fn local_zip_release() -> Result<Option<(PathBuf, Release)>> {
    let Some(path) = std::env::var_os("PYAPPIFY_LOCAL_ZIP") else {
        return Ok(None);
    };
    let version = std::env::var("PYAPPIFY_LOCAL_ZIP_VERSION")
        .context("Set PYAPPIFY_LOCAL_ZIP_VERSION for a local ZIP test")?;
    if crate::git::compare_version_tags(&version, &version).is_none() {
        bail!("Invalid local ZIP version");
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() || !path.is_file() {
        bail!("PYAPPIFY_LOCAL_ZIP must point to a local ZIP file");
    }
    Ok(Some((
        path,
        Release {
            version_name: version,
            url: None,
            release_note: "Local ZIP test".into(),
            sha256: None,
        },
    )))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn local_zip_inputs_avoid_every_tree_that_recovery_or_installation_can_remove() {
        let root = Path::new(r"\\?\C:\test\installation");
        for path in [
            r"C:\test\installation\data\apps\ok-nte\working\test.zip",
            r"C:\test\installation\data\apps\other-app\repo\test.zip",
            r"C:\test\installation\DATA\APPS\ok-nte\test.zip",
            r"C:\test\installation\.pyappify-update\staged\test.zip",
            r"C:\test\installation\.pyappify-update-completed-abc\backup\test.zip",
        ] {
            assert!(
                validate_local_zip_location(Path::new(path), root).is_err(),
                "unsafe local input was accepted: {path}"
            );
        }
        for path in [
            r"C:\test\packages\test.zip",
            r"C:\test\installation\packages\test.zip",
            r"C:\test\installation-other\data\apps\ok-nte\test.zip",
            r"C:\test\installation\data\apps-other\test.zip",
            r"C:\test\installation\packages\.pyappify-update\test.zip",
            r"C:\test\installation\PYAPPIFY.YML",
        ] {
            assert!(
                validate_local_zip_location(Path::new(path), root).is_ok(),
                "independent local input was rejected: {path}"
            );
        }
    }
}
