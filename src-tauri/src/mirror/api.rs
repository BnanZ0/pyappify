//! MirrorChyan release queries; a CDK is optional for version checks.
use super::credentials::read_cdk;
#[cfg(debug_assertions)]
use super::download::local_zip_release;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::time::Duration;

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

pub(super) fn client(timeout: Duration) -> Result<reqwest::Client> {
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
    let current_version =
        if app.installed && app.installed_source() == super::UpdateSource::Mirrorchyan {
            Some(
                crate::frozen::program_files::ProgramFiles::read(
                    &crate::utils::path::get_app_working_dir_path(&app.name),
                )?
                .version,
            )
        } else {
            None
        };
    Api::new(app)?.lookup(current_version.as_deref()).await
}

pub(super) struct Api {
    pub client: reqwest::Client,
    cdk: Option<String>,
    pub resource_id: String,
    channel: String,
    arch: &'static str,
}
impl Api {
    pub fn new(app: &crate::app::App) -> Result<Self> {
        let config = app
            .mirrorchyan
            .as_ref()
            .context("MirrorChyan is not configured for this application")?;
        config.validate_resource_id()?;
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

        Ok(Self {
            client: client(Duration::from_secs(1800))?,
            cdk: read_cdk()?,
            resource_id: config.resource_id.clone(),
            channel: channel.to_string(),
            arch,
        })
    }
    pub fn require_cdk(&self) -> Result<()> {
        if self.cdk.as_deref().is_none_or(|value| value.is_empty()) {
            bail!("Configure a MirrorChyan CDK before installing or updating");
        }
        Ok(())
    }
    pub async fn lookup(&self, current_version: Option<&str>) -> Result<Release> {
        #[cfg(debug_assertions)]
        if let Some((_, release)) = local_zip_release()? {
            return Ok(release);
        }
        // Some resources are uploaded without an OS/architecture designation. Try the
        // exact launcher platform first, then retry that generic resource on code 8001.
        for include_platform in [true, false] {
            // Never attach reqwest errors: they can contain the CDK-bearing request URL.
            let mut url = reqwest::Url::parse(&format!(
                "https://mirrorchyan.com/api/resources/{}/latest",
                self.resource_id
            ))?;
            {
                let mut query = url.query_pairs_mut();
                query
                    .append_pair("channel", &self.channel)
                    .append_pair("user_agent", "PyAppify");
                // Omit the baseline entirely to request a complete package. v0.0.0
                // can itself be a recorded server baseline and is not a safe sentinel.
                if let Some(version) = current_version {
                    query.append_pair("current_version", version);
                }
                if let Some(cdk) = self.cdk.as_deref() {
                    query.append_pair("cdk", cdk);
                }
                if include_platform {
                    query
                        .append_pair("os", "win")
                        .append_pair("arch", self.arch);
                }
            }
            let response = self
                .client
                .get(url)
                .timeout(Duration::from_secs(45))
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
