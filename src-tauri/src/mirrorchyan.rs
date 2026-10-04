//! MirrorChyan ZIP transport and Windows-account encrypted CDK storage.
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateSource {
    #[default]
    Git,
    Mirrorchyan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MirrorConfig {
    pub resource_id: String,
    #[serde(default = "stable_channel")]
    pub stable_channel: String,
    #[serde(default)]
    pub prerelease_channel: Option<String>,
}
fn stable_channel() -> String {
    "stable".into()
}

#[derive(Deserialize)]
struct Response {
    code: i64,
    data: Option<serde_json::Value>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub version_name: String,
    pub url: Option<String>,
    #[serde(default, deserialize_with = "nullable_note")]
    pub release_note: String,
    pub sha256: Option<String>,
}

fn nullable_note<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Serialize)]
pub struct Progress<'a> {
    pub app_name: &'a str,
    pub phase: &'a str,
    pub downloaded: u64,
    pub total: Option<u64>,
}
pub fn progress(app: &str, phase: &str, downloaded: u64, total: Option<u64>) {
    crate::emitter::emit(
        "mirror-update-progress",
        Progress {
            app_name: app,
            phase,
            downloaded,
            total,
        },
    );
}

pub fn private_dir() -> Result<PathBuf> {
    let root = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?;
    let install = crate::utils::path::get_cwd().canonicalize()?;
    let key = format!(
        "{:x}",
        Sha256::digest(install.to_string_lossy().to_lowercase().as_bytes())
    );
    Ok(PathBuf::from(root)
        .join("PyAppify")
        .join("updates")
        .join(key))
}

#[cfg(windows)]
fn protect(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::{Foundation::LocalFree, Security::Cryptography::*};
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into()?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output: CRYPT_INTEGER_BLOB = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        bail!("Unable to access the encrypted MirrorChyan CDK. Save the CDK again.");
    }
    let result =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData as _);
    }
    Ok(result)
}
#[cfg(not(windows))]
fn protect(_: &[u8], _: bool) -> Result<Vec<u8>> {
    bail!("MirrorChyan CDK storage requires Windows")
}

fn read_cdk() -> Result<Option<String>> {
    let path = private_dir()?.join("cdk.bin");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(protect(
        &std::fs::read(path)?,
        false,
    )?)?))
}

#[tauri::command]
pub async fn mirrorchyan_has_cdk() -> Result<bool, String> {
    read_cdk()
        .map(|cdk| cdk.is_some_and(|key| !key.trim().is_empty()))
        .map_err(|e| e.to_string())
}

pub fn require_cdk() -> Result<()> {
    #[cfg(debug_assertions)]
    if local_zip_release()?.is_some() {
        return Ok(());
    }
    check_cdk(read_cdk()?.as_deref())
}

fn check_cdk(cdk: Option<&str>) -> Result<()> {
    if cdk.is_none_or(|key| key.trim().is_empty()) {
        bail!("Configure a MirrorChyan CDK in Settings before installing or updating.");
    }
    Ok(())
}
#[tauri::command]
pub async fn mirrorchyan_set_cdk(cdk: String) -> Result<(), String> {
    (|| -> Result<()> {
        let dir = private_dir()?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("cdk.bin");
        if cdk.trim().is_empty() {
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        } else {
            crate::utils::file::atomic_write(&path, &protect(cdk.trim().as_bytes(), true)?)?;
        }
        Ok(())
    })()
    .map_err(|e| e.to_string())
}

fn client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() != "https" || attempt.previous().len() >= 5 {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

pub async fn latest(app: &crate::app::App) -> Result<Release> {
    #[cfg(debug_assertions)]
    if let Some((_, release)) = local_zip_release()? {
        return Ok(release);
    }
    lookup(app, app.current_version.as_deref()).await
}

pub async fn latest_for_install(app: &crate::app::App) -> Result<Release> {
    cancellable(latest(app)).await?
}

async fn lookup(app: &crate::app::App, current_version: Option<&str>) -> Result<Release> {
    let config = app
        .mirrorchyan
        .as_ref()
        .context("MirrorChyan is not configured for this application")?;
    if config.resource_id.is_empty()
        || !config
            .resource_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        bail!("Invalid MirrorChyan resource_id");
    }
    let channel =
        if app.effective_update_method() == crate::app::UPDATE_METHOD_OPTION_AUTO_PRE_RELEASE {
            config
                .prerelease_channel
                .as_deref()
                .unwrap_or(&config.stable_channel)
        } else {
            &config.stable_channel
        };
    let arch = platform_arch()?;
    let cdk = read_cdk()?;

    // Some resources are uploaded without an OS/architecture designation. Try the
    // exact launcher platform first, then retry that generic resource on code 8001.
    for include_platform in [true, false] {
        // Never attach reqwest errors: they can contain the CDK-bearing request URL.
        let mut url = reqwest::Url::parse(&format!(
            "https://mirrorchyan.com/api/resources/{}/latest",
            config.resource_id
        ))?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("channel", channel)
                .append_pair("user_agent", "PyAppify");
            // Omit the baseline entirely to request a complete package. v0.0.0
            // can itself be a recorded server baseline and is not a safe sentinel.
            if let Some(version) = current_version {
                query.append_pair("current_version", version);
            }
            if let Some(cdk) = cdk.as_deref() {
                query.append_pair("cdk", cdk);
            }
            if include_platform {
                query.append_pair("os", "win").append_pair("arch", arch);
            }
        }
        let response = client(Duration::from_secs(45))?
            .get(url)
            .send()
            .await
            .map_err(|_| {
                anyhow::anyhow!("MirrorChyan version check failed; check your connection")
            })?;
        let status = response.status();
        let body: Response = response.json().await.map_err(|_| {
            anyhow::anyhow!("Invalid MirrorChyan response (HTTP {})", status.as_u16())
        })?;
        if include_platform && body.code == 8001 {
            continue;
        }
        return parse_response(body, status.is_success());
    }
    unreachable!("the generic MirrorChyan request always returns from the loop")
}

fn platform_arch() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x64"),
        "aarch64" => Ok("arm64"),
        _ => bail!("Unsupported MirrorChyan architecture"),
    }
}

fn parse_response(body: Response, http_ok: bool) -> Result<Release> {
    if body.code != 0 || !http_ok {
        let hint = match body.code {
            7001 => "The CDK has expired.",
            7002 => "The CDK is invalid.",
            7003 => "The CDK download quota is exhausted.",
            7004 => "The CDK does not match this resource.",
            7005 => "The CDK has been blocked.",
            8001 => "No generic resource or resource for this Windows architecture was found.",
            8002 => "MirrorChyan rejected the operating system value.",
            8003 => "MirrorChyan rejected the architecture value.",
            8004 => "MirrorChyan rejected the configured release channel.",
            _ => "Check the resource settings or contact MirrorChyan support.",
        };
        bail!("MirrorChyan request failed (code {}). {}", body.code, hint);
    }
    let release: Release =
        serde_json::from_value(body.data.context("MirrorChyan returned no release")?)
            .map_err(|_| anyhow::anyhow!("Invalid MirrorChyan release metadata"))?;
    if crate::git::compare_version_tags(&release.version_name, &release.version_name).is_none() {
        bail!("Unsupported MirrorChyan version format");
    }
    Ok(release)
}

pub fn newer(release: &Release, current: Option<&str>) -> bool {
    current
        .map(|v| {
            crate::git::compare_version_tags(&release.version_name, v)
                == Some(std::cmp::Ordering::Greater)
        })
        .unwrap_or(true)
}

async fn cancellable<F: Future>(future: F) -> Result<F::Output> {
    tokio::pin!(future);
    loop {
        if crate::app_service::app_operation_cancelled() {
            bail!("Operation cancelled by user");
        }
        tokio::select! {
            result = &mut future => return Ok(result),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
}

/// This directory is private to the install path, alongside the existing CDK.
/// No URL or CDK is included in download errors or filenames.
pub async fn download(
    app: &crate::app::App,
    target: &str,
    baseline: Option<&str>,
    destination: &Path,
) -> Result<Release> {
    #[cfg(debug_assertions)]
    if let Some((source, release)) = local_zip_release()? {
        if release.version_name != target {
            bail!("The local ZIP version changed");
        }
        let total = tokio::fs::metadata(&source).await?.len();
        progress(&app.name, "downloading", 0, Some(total));
        let mut input = tokio::fs::File::open(source).await?;
        let mut output = tokio::fs::File::create(destination).await?;
        cancellable(tokio::io::copy(&mut input, &mut output)).await??;
        progress(&app.name, "downloaded", total, Some(total));
        return Ok(release);
    }
    for attempt in 0..2 {
        let release = cancellable(lookup(app, baseline)).await??;
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
        let response = cancellable(client(Duration::from_secs(1800))?.get(parsed).send())
            .await?
            .map_err(|_| anyhow::anyhow!("ZIP download failed; check your connection and retry"))?;
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
    let mut digest = Sha256::new();
    let mut size = 0u64;
    let mut last_emit = std::time::Instant::now();
    while let Some(chunk) = cancellable(stream.next()).await? {
        let chunk =
            chunk.map_err(|_| anyhow::anyhow!("ZIP download interrupted; retry the update"))?;
        output.write_all(&chunk).await?;
        digest.update(&chunk);
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
    if expected.is_some_and(|hash| !format!("{:x}", digest.finalize()).eq_ignore_ascii_case(hash)) {
        bail!("ZIP SHA-256 checksum mismatch");
    }
    progress(app_name, "downloaded", size, total);
    Ok(())
}

/// Local ZIP tests use the same preparation/apply path as remote updates.
#[cfg(debug_assertions)]
fn local_zip_release() -> Result<Option<(PathBuf, Release)>> {
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

#[cfg(test)]
mod tests {
    #[test]
    fn missing_key_is_rejected_before_starting_a_download() {
        assert!(super::check_cdk(None).is_err());
        assert!(super::check_cdk(Some("  ")).is_err());
        assert!(super::check_cdk(Some("key-present")).is_ok());
    }
    use super::*;

    async fn response(bytes: &[u8], declared_size: usize) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = bytes.to_vec();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {declared_size}\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        });
        reqwest::Client::new()
            .get(format!("http://{address}/zip"))
            .send()
            .await
            .unwrap()
    }

    fn directory() -> PathBuf {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/mirror-transport-tests")
            .join(format!("{:x}", rand::random::<u64>()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn verifies_streamed_checksum_and_rejects_mismatch() {
        let root = directory();
        let bytes = b"ZIP transport test";
        let digest = format!("{:x}", Sha256::digest(bytes));
        let path = root.join("download");
        receive_zip(
            "sample",
            response(bytes, bytes.len()).await,
            Some(&digest),
            &path,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let error = receive_zip(
            "sample",
            response(bytes, bytes.len()).await,
            Some(&"0".repeat(64)),
            &path,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rejects_truncated_download_without_exposing_request_url() {
        let root = directory();
        let error = receive_zip(
            "sample",
            response(b"partial", 100).await,
            None,
            &root.join("download"),
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().contains("http"));
        assert!(
            error.to_string().contains("interrupted") || error.to_string().contains("Incomplete")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nullable_notes_and_errors_do_not_echo_cdk_or_server_data() {
        let release = parse_response(
            Response {
                code: 0,
                data: Some(serde_json::json!({"version_name":"v1.2.3","release_note":null})),
            },
            true,
        )
        .unwrap();
        assert!(release.release_note.is_empty());
        let error = parse_response(Response{code:7002,data:Some(serde_json::json!({"cdk":"PRIVATE-CDK","url":"https://example.com/?cdk=PRIVATE-CDK"}))},false).unwrap_err();
        assert!(error.to_string().contains("7002"));
        assert!(!error.to_string().contains("PRIVATE-CDK"));
    }
}
