//! Application body information derived from its YAML and program manifest.
use super::program_files::{ProgramFiles, MANIFEST_FILE};
use crate::app::{parse_app_template, App};
pub(crate) use crate::extensions::file_operations::{key, read_json, relative, safe_join, within};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io::Read, path::Path};

pub const WORKING_CONFIG_FILE: &str = "pyappify.yml";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Package {
    pub app_name: String,
    pub version: String,
    pub resource_id: Option<String>,
    pub executable: String,
    pub profiles: Vec<String>,
}

pub struct Expected<'a> {
    pub app_name: &'a str,
    pub resource_id: &'a str,
    pub version: &'a str,
}

impl Package {
    pub(crate) fn ignores_launcher(&self, value: &str) -> bool {
        !value.contains('/') && value.to_ascii_lowercase().ends_with(".exe")
    }
    pub fn base(&self) -> String {
        format!("data/apps/{}", self.app_name)
    }
    pub fn executable(&self) -> &str {
        &self.executable
    }
    pub fn validate(&self, expected: &Expected<'_>) -> Result<()> {
        if self.app_name != expected.app_name
            || self.resource_id.as_deref() != Some(expected.resource_id)
            || self.version != expected.version
        {
            bail!("ZIP release identity or version does not match this application");
        }
        Ok(())
    }
    pub fn payload_present(&self, root: &Path) -> Result<bool> {
        let working = safe_join(root, &format!("{}/working", self.base()))?;
        let internal = safe_join(&working, "_internal")?;
        for (path, directory) in [
            (safe_join(&working, WORKING_CONFIG_FILE)?, false),
            (safe_join(&working, self.executable())?, false),
            (internal.clone(), true),
        ] {
            match fs::metadata(&path) {
                Ok(metadata)
                    if if directory {
                        metadata.is_dir()
                    } else {
                        metadata.is_file()
                    } => {}
                Ok(_) => return Ok(false),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Unable to inspect {}", path.display()))
                }
            }
        }
        for entry in fs::read_dir(internal)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with("python") && name.ends_with(".dll") && entry.file_type()?.is_file()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub fn allows(&self, value: &str, directory: bool) -> bool {
        if relative(value).is_err() {
            return false;
        }
        if self.ignores_launcher(value) {
            return !directory;
        }
        let base = self.base();
        let working = format!("{base}/working");
        if directory && ["data", "data/apps", &base].contains(&value) {
            return true;
        }
        within(value, &working)
            && (directory || key(value) != key(&working))
            && !value.split('/').any(|p| p.eq_ignore_ascii_case(".git"))
            && !key(value).ends_with("/.pip_update_needed.tmp")
    }
    pub(crate) fn from_config(config: &App, version: &str, executable: &str) -> Result<Self> {
        if crate::git::compare_version_tags(version, version).is_none() {
            bail!("Invalid frozen package version");
        }
        relative(executable)?;
        if executable.contains('/') || !executable.to_ascii_lowercase().ends_with(".exe") {
            bail!("Invalid application executable");
        }
        let app_name = config.name.clone();
        if relative(&app_name)?.components().count() != 1 {
            bail!("Invalid application name");
        }
        if config.update_source == crate::mirror::UpdateSource::Mirrorchyan
            && config.mirrorchyan.is_none()
        {
            bail!("MirrorChyan updates require mirrorchyan.resource_id");
        }
        let resource_id = config
            .mirrorchyan
            .as_ref()
            .map(|mirror| {
                mirror.validate_resource_id()?;
                Ok::<_, anyhow::Error>(mirror.resource_id.clone())
            })
            .transpose()?;
        let profiles = config
            .profiles
            .iter()
            .map(|profile| {
                if profile.name.is_empty() {
                    bail!("Invalid application profile");
                }
                Ok(profile.name.clone())
            })
            .collect::<Result<Vec<_>>>()?;
        if profiles.is_empty() {
            bail!("Application configuration has no profiles");
        }
        Ok(Self {
            app_name,
            version: version.into(),
            resource_id,
            executable: executable.into(),
            profiles,
        })
    }
}

pub fn read_package(root: &Path, app_name: &str) -> Result<Option<(Package, App)>> {
    let working = safe_join(root, &format!("data/apps/{app_name}/working"))?;
    for (path, directory) in [(&working, true), (&working.join(MANIFEST_FILE), false)] {
        match fs::metadata(path) {
            Ok(metadata)
                if if directory {
                    metadata.is_dir()
                } else {
                    metadata.is_file()
                } => {}
            Ok(_) => bail!("Unexpected application body path type: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("Unable to inspect {}", path.display()))
            }
        }
    }
    let program = ProgramFiles::read(&working)?;
    let config_path = safe_join(&working, WORKING_CONFIG_FILE)?;
    let content = match fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to read {}", config_path.display()))
        }
    };
    let config = parse_app_template(&content)?;
    config
        .mirrorchyan
        .as_ref()
        .context("Launcher frozen installations require mirrorchyan.resource_id")?
        .validate_resource_id()?;
    let package = Package::from_config(&config, &program.version, &program.executable)?;
    if package.app_name != app_name {
        bail!("Application body belongs to a different application");
    }
    Ok(Some((package, config)))
}

pub(crate) fn read_zip_package(
    archive: &mut zip::ZipArchive<fs::File>,
    app_name: &str,
    installed_root: Option<&Path>,
) -> Result<(Package, App, ProgramFiles)> {
    let working = format!("data/apps/{app_name}/working");
    let program = ProgramFiles::from_reader(
        archive
            .by_name(&format!("{working}/{MANIFEST_FILE}"))
            .context("Missing pyappify-files.json in ZIP")?,
    )?;
    let config = match archive.by_name(&format!("{working}/{WORKING_CONFIG_FILE}")) {
        Ok(mut file) => {
            let mut bytes = String::new();
            file.read_to_string(&mut bytes)?;
            parse_app_template(&bytes)?
        }
        Err(zip::result::ZipError::FileNotFound) => {
            let root = installed_root.context("Missing application YAML in full ZIP")?;
            parse_app_template(&fs::read_to_string(safe_join(
                root,
                &format!("{working}/{WORKING_CONFIG_FILE}"),
            )?)?)?
        }
        Err(error) => return Err(error.into()),
    };
    Ok((
        Package::from_config(&config, &program.version, &program.executable)?,
        config,
        program,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_body_is_absent_but_invalid_paths_and_manifest_are_errors() {
        let root =
            std::env::temp_dir().join(format!("pyappify-body-read-{:x}", rand::random::<u64>()));
        let working = root.join("data/apps/example/working");
        fs::create_dir_all(working.parent().unwrap()).unwrap();
        assert!(read_package(&root, "example").unwrap().is_none());
        fs::write(&working, b"not a directory").unwrap();
        assert!(read_package(&root, "example")
            .unwrap_err()
            .to_string()
            .contains("path type"));
        fs::remove_file(&working).unwrap();
        fs::create_dir(&working).unwrap();
        fs::write(working.join(MANIFEST_FILE), b"invalid manifest").unwrap();
        assert!(read_package(&root, "example").is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn body_scope_excludes_root_metadata_and_other_applications() {
        let package = Package {
            app_name: "example".into(),
            version: "v1".into(),
            resource_id: Some("example".into()),
            executable: "application.exe".into(),
            profiles: vec!["default".into()],
        };
        for name in [
            "renamed-launcher.exe",
            "data/apps/example/working/pyappify.yml",
            "data/apps/example/working/application.exe",
        ] {
            assert!(package.allows(name, false));
        }
        for name in [
            "pyappify-release.json",
            "pyappify.yml",
            "data/apps/other/working/pyappify.yml",
            "data/apps/example/working/../pyappify.yml",
        ] {
            assert!(!package.allows(name, false));
        }
    }
}
