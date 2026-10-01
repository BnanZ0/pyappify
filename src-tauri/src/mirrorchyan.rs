//! MirrorChyan release lookup and Windows-account encrypted CDK storage.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};

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
