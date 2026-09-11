//! MirrorChyan transports complete NSIS installers; it never synchronizes Git or pip.
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
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
        "installer-update-progress",
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
    bail!("MirrorChyan installer updates require Windows")
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
    private_dir()
        .map(|p| p.join("cdk.bin").exists())
        .map_err(|e| e.to_string())
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
            crate::installer_update::atomic_write(&path, &protect(cdk.trim().as_bytes(), true)?)?;
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
                .append_pair(
                    "current_version",
                    app.current_version.as_deref().unwrap_or("v0.0.0"),
                )
                .append_pair("channel", channel)
                .append_pair("user_agent", "PyAppify");
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
        _ => bail!("Unsupported installer architecture"),
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

pub async fn download(app: &crate::app::App, target: &str, dir: &Path) -> Result<PathBuf> {
    for attempt in 0..2 {
        let release = latest(app).await?;
        if release.version_name != target {
            bail!("The latest release changed. Check for updates again.");
        }
        let url = release
            .url
            .context("No installer download available. Configure a valid MirrorChyan CDK.")?;
        let parsed = reqwest::Url::parse(&url)
            .map_err(|_| anyhow::anyhow!("Invalid installer download URL"))?;
        if parsed.scheme() != "https" {
            bail!("Installer download requires HTTPS");
        }
        let response = client(Duration::from_secs(1800))?
            .get(parsed)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Installer download failed; retry the update"))?;
        if attempt == 0 && matches!(response.status().as_u16(), 401 | 403 | 410) {
            continue;
        }
        if !response.status().is_success() {
            bail!(
                "Installer download failed (HTTP {})",
                response.status().as_u16()
            );
        }
        return receive_installer(&app.name, response, release.sha256, dir).await;
    }
    bail!("Installer download URL expired; retry the update")
}

async fn receive_installer(
    app_name: &str,
    response: reqwest::Response,
    expected_sha256: Option<String>,
    dir: &Path,
) -> Result<PathBuf> {
    let total = response.content_length();
    let partial = dir.join("setup.downloading");
    let mut file = tokio::fs::File::create(&partial).await?;
    let mut stream = response.bytes_stream();
    let mut size = 0u64;
    let mut digest = Sha256::new();
    let mut last_emit = std::time::Instant::now();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|_| anyhow::anyhow!("Installer download interrupted; retry the update"))?;
        file.write_all(&chunk).await?;
        digest.update(&chunk);
        size += chunk.len() as u64;
        if last_emit.elapsed() >= Duration::from_millis(200) {
            progress(app_name, "downloading", size, total);
            last_emit = std::time::Instant::now();
        }
    }
    file.sync_all().await?;
    drop(file);
    if total.is_some_and(|n| n != size) {
        bail!("Incomplete installer download");
    }
    if let Some(expected) = expected_sha256 {
        if !format!("{:x}", digest.finalize()).eq_ignore_ascii_case(&expected) {
            bail!("Installer SHA-256 checksum mismatch");
        }
    }
    validate_pe(&partial)?;
    let ready = dir.join("setup.exe");
    tokio::fs::rename(&partial, &ready).await?;
    progress(app_name, "downloaded", size, total);
    Ok(ready)
}

pub fn validate_pe(path: &Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos)
        .context("Installer is not a Windows executable")?;
    if &dos[..2] != b"MZ" {
        bail!("Installer is not a Windows executable");
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into()?) as u64;
    if offset < 64 || offset + 24 > len {
        bail!("Invalid executable header");
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header)?;
    if &header[..4] != b"PE\0\0" || u16::from_le_bytes([header[6], header[7]]) == 0 {
        bail!("Invalid Windows executable");
    }
    let sections = u16::from_le_bytes([header[6], header[7]]) as u64;
    let optional_size = u16::from_le_bytes([header[20], header[21]]) as u64;
    let characteristics = u16::from_le_bytes([header[22], header[23]]);
    if optional_size < 96
        || sections > 96
        || characteristics & 2 == 0
        || characteristics & 0x2000 != 0
        || offset + 24 + optional_size + sections * 40 > len
    {
        bail!("Invalid or truncated executable sections");
    }
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic)?;
    if !matches!(u16::from_le_bytes(magic), 0x10b | 0x20b) {
        bail!("Invalid executable optional header");
    }
    file.seek(SeekFrom::Start(offset + 24 + optional_size))?;
    for _ in 0..sections {
        let mut section = [0u8; 40];
        file.read_exact(&mut section)?;
        let size = u32::from_le_bytes(section[16..20].try_into()?) as u64;
        let start = u32::from_le_bytes(section[20..24].try_into()?) as u64;
        if start + size > len {
            bail!("Truncated executable section data");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            Self(crate::installer_update::new_task_dir().unwrap())
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            for name in ["setup.downloading", "setup.exe", "bad.exe"] {
                let _ = std::fs::remove_file(self.0.join(name));
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    async fn response(body: Vec<u8>, declared_size: usize) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            let _ = socket.read(&mut buffer).await;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {declared_size}\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}/download?secret=do-not-log"))
            .send()
            .await
            .unwrap()
    }
    fn executable_fixture() -> Vec<u8> {
        let mut bytes = vec![0; 512];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&0x14cu16.to_le_bytes());
        bytes[70..72].copy_from_slice(&1u16.to_le_bytes());
        bytes[84..86].copy_from_slice(&96u16.to_le_bytes());
        bytes[86..88].copy_from_slice(&0x102u16.to_le_bytes());
        bytes[88..90].copy_from_slice(&0x10bu16.to_le_bytes());
        bytes
    }
    #[tokio::test]
    async fn successful_download_is_published_only_after_validation() {
        let dir = TestDir::new();
        let body = executable_fixture();
        let digest = format!("{:x}", Sha256::digest(&body));
        let response = response(body.clone(), body.len()).await;
        let ready = receive_installer("test", response, Some(digest), &dir.0)
            .await
            .unwrap();
        assert_eq!(std::fs::read(ready).unwrap(), body);
        assert!(!dir.0.join("setup.downloading").exists());
    }
    #[tokio::test]
    async fn html_and_checksum_failures_never_publish_an_installer() {
        for (body, checksum) in [
            (vec![b'<'; 512], None),
            (executable_fixture(), Some("0".repeat(64))),
        ] {
            let dir = TestDir::new();
            let response = response(body.clone(), body.len()).await;
            assert!(receive_installer("test", response, checksum, &dir.0)
                .await
                .is_err());
            assert!(!dir.0.join("setup.exe").exists());
        }
    }
    #[tokio::test]
    async fn truncated_network_body_is_rejected_without_leaking_url() {
        let dir = TestDir::new();
        let response = response(executable_fixture(), 1024).await;
        let error = receive_installer("test", response, None, &dir.0)
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("do-not-log"));
        assert!(!dir.0.join("setup.exe").exists());
    }
    #[test]
    fn executable_validation_rejects_out_of_bounds_headers() {
        let dir = TestDir::new();
        let mut bytes = executable_fixture();
        bytes[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
        let path = dir.0.join("bad.exe");
        std::fs::write(&path, bytes).unwrap();
        assert!(validate_pe(&path).is_err());
        #[cfg(windows)]
        validate_pe(&std::env::current_exe().unwrap()).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip_and_corrupt_ciphertext() {
        let secret = b"test-key-never-persisted";
        let encrypted = protect(secret, true).unwrap();
        assert_ne!(encrypted, secret);
        assert_eq!(protect(&encrypted, false).unwrap(), secret);
        assert!(protect(b"invalid ciphertext", false).is_err());
    }
    #[test]
    fn response_errors_do_not_echo_secrets() {
        let value: Response =
            serde_json::from_str(r#"{"code":7001,"msg":"secret-cdk","data":{}}"#).unwrap();
        assert!(!parse_response(value, false)
            .err()
            .unwrap()
            .to_string()
            .contains("secret-cdk"));
    }
    #[test]
    fn platform_names_match_mirrorchyan_api() {
        #[cfg(target_arch = "x86_64")]
        assert_eq!(platform_arch().unwrap(), "x64");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(platform_arch().unwrap(), "arm64");
    }
    #[test]
    fn resource_not_found_error_identifies_platform_resource() {
        let value: Response = serde_json::from_str(r#"{"code":8001,"data":null}"#).unwrap();
        let error = parse_response(value, false).unwrap_err().to_string();
        assert!(error.contains("Windows architecture"));
        assert!(!error.contains("CDK"));
    }
    #[test]
    fn no_url_still_allows_version_check() {
        let value: Response =
            serde_json::from_str(r#"{"code":0,"data":{"version_name":"v1.2.0"}}"#).unwrap();
        let release = parse_response(value, true).unwrap();
        assert!(release.url.is_none());
        assert!(newer(&release, Some("v1.1.0")));
        assert!(!newer(&release, Some("v1.2.0")));
        let nullable: Response = serde_json::from_str(
            r#"{"code":0,"data":{"version_name":"v1.2.0","release_note":null}}"#,
        )
        .unwrap();
        assert!(parse_response(nullable, true)
            .unwrap()
            .release_note
            .is_empty());
    }
    #[test]
    fn old_yaml_defaults_to_git() {
        let app: crate::app::App = serde_yaml::from_str("name: sample\nprofiles: []").unwrap();
        assert_eq!(app.update_source, UpdateSource::Git);
        assert!(app.mirrorchyan.is_none());
    }
}
