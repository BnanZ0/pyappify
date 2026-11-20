//! Build and package PyInstaller applications through the launcher CLI.
use anyhow::{bail, Context, Result};
use serde_yaml::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use zip::write::SimpleFileOptions;

use super::{
    package,
    program_files::{self, ProgramFiles},
};

pub fn package(args: &[String]) -> Result<()> {
    if !(6..=7).contains(&args.len()) {
        bail!("Usage: frozen_pack <clean-onedir> <launcher.exe> <app.yml> <application.exe> <version> <full.zip> [body.zip]");
    }
    let bundle = std::path::absolute(&args[0])?;
    let launcher = Path::new(&args[1]);
    let yaml = serde_yaml::from_str(&fs::read_to_string(&args[2])?)?;
    let (yaml, manifest) = preflight(yaml, launcher, &args[3], &args[4])?;
    let outputs = package_outputs(
        &bundle,
        &args[5],
        args.get(6).map(String::as_str),
        manifest.resource_id.is_some(),
    )?;
    package_body(&bundle, launcher, outputs, &yaml, &manifest)
}

fn preflight(
    mut yaml: Value,
    launcher: &Path,
    executable: &str,
    version: &str,
) -> Result<(Value, package::Package)> {
    if yaml
        .get("mirrorchyan")
        .is_some_and(|config| !config.is_null())
    {
        // Full distributions keep the original launcher/Mirror installation route.
        yaml["update_source"] = Value::String("mirrorchyan".into());
    }
    let app = crate::app::parse_app_template(&serde_yaml::to_string(&yaml)?)?;
    let manifest = package::Package::from_config(&app, version, executable)?;
    if manifest.resource_id.is_some() {
        super::launcher_identity::validate_file(launcher, &manifest.app_name)?;
    }
    Ok((yaml, manifest))
}

fn package_body(
    bundle: &Path,
    launcher: &Path,
    (output, body_output): (Option<PathBuf>, PathBuf),
    yaml: &Value,
    manifest: &package::Package,
) -> Result<()> {
    program_files::reject_link(bundle)?;
    if bundle.file_name().is_some_and(|name| name == "working")
        && bundle
            .parent()
            .is_some_and(|parent| parent.join("app.json").is_file())
    {
        bail!("Package a clean build directory; an installed working directory contains local user files");
    }
    if !bundle.join(&manifest.executable).is_file() || !bundle.join("_internal").is_dir() {
        bail!("Incomplete onedir: missing application executable or _internal");
    }
    let yaml_bytes = serde_yaml::to_string(yaml)?;
    // This runs after build.ps1 has copied all external application resources.
    let body_config = bundle.join(package::WORKING_CONFIG_FILE);
    if body_config.exists() {
        program_files::reject_link(&body_config)?;
    }
    fs::write(body_config, &yaml_bytes)?;
    let program_files =
        ProgramFiles::capture_clean_body(bundle, &manifest.version, &manifest.executable)?;
    let working = format!("{}/working", manifest.base());
    for name in &program_files.files {
        if !manifest.allows(&format!("{working}/{name}"), false) {
            bail!("Unsupported application body file: {name}");
        }
    }
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(6));
    let body_partial = body_output.with_extension("zip.building");
    let mut body = zip::ZipWriter::new(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&body_partial)?,
    );
    for name in &program_files.files {
        body.start_file(name, options)?;
        std::io::copy(&mut fs::File::open(bundle.join(name))?, &mut body)?;
    }
    body.finish()?.sync_all()?;
    crate::extensions::file_operations::move_path(
        &body_partial,
        &body_output,
        "Frozen publish body ZIP",
        &|| false,
    )?;

    if let Some(output) = &output {
        // Reuse the finished body ZIP, including exactly the same program manifest.
        let partial = output.with_extension("zip.building");
        let mut archive = zip::ZipWriter::new(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial)?,
        );
        archive.start_file(
            launcher
                .file_name()
                .context("Missing launcher filename")?
                .to_string_lossy(),
            options,
        )?;
        std::io::copy(&mut fs::File::open(launcher)?, &mut archive)?;
        let mut body = zip::ZipArchive::new(fs::File::open(&body_output)?)?;
        for index in 0..body.len() {
            let entry = body.by_index(index)?;
            let name = format!("{working}/{}", entry.name());
            archive.raw_copy_file_rename(entry, name)?;
        }
        archive.finish()?.sync_all()?;
        if output.exists() {
            bail!("Output ZIP already exists");
        }
        crate::extensions::file_operations::move_path(
            &partial,
            output,
            "Frozen publish full ZIP",
            &|| false,
        )?;
    }
    let published = output.as_ref().unwrap_or(&body_output);
    let files = program_files.files.len() + usize::from(output.is_some());
    let mut digest = Sha256::new();
    let mut input = fs::File::open(published)?;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    println!(
        "{}",
        serde_json::json!({"zip": published, "body_zip": body_output, "body_files": program_files.files.len(), "bytes": fs::metadata(published)?.len(), "files": files, "sha256": format!("{:x}", digest.finalize()), "package": manifest})
    );
    Ok(())
}

/// Run before Tauri initialization: packaging must not open the launcher UI.
pub fn try_run(args: &[String]) -> Option<Result<()>> {
    if matches!(args.get(1).map(String::as_str), Some("-c" | "--command"))
        && args.get(2).map(String::as_str) == Some("frozen-zip")
    {
        Some(build_and_package(&args[3..]))
    } else {
        None
    }
}

fn build_and_package(args: &[String]) -> Result<()> {
    if !(3..=4).contains(&args.len()) {
        bail!("Usage: pyappify -c frozen-zip <app.yml> <version> <full.zip> [body.zip]");
    }
    if !cfg!(windows) {
        bail!("Frozen PyInstaller builds currently require Windows");
    }
    let config = std::path::absolute(&args[0])?;
    let project = config
        .parent()
        .context("Application config has no parent directory")?;
    let yaml: Value = serde_yaml::from_str(&fs::read_to_string(&config)?)?;
    let packaging = yaml
        .get("packaging")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("Set packaging to the application's packaging directory in pyappify.yml")?;
    let directory = project.join(packaging);
    let specs = fs::read_dir(&directory)
        .with_context(|| format!("Cannot read packaging directory: {}", directory.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("spec"))
        })
        .collect::<Vec<_>>();
    if specs.len() != 1 {
        bail!(
            "The packaging directory must contain exactly one .spec file; found {}",
            specs.len()
        );
    }
    let spec = &specs[0];
    let name = spec
        .file_stem()
        .and_then(|name| name.to_str())
        .context("Invalid spec filename")?;
    let requirements = ["requirements-build.txt", "requirements.txt"]
        .into_iter()
        .map(|name| directory.join(name))
        .find(|path| path.is_file())
        .context("Missing requirements-build.txt or requirements.txt in the packaging directory")?;
    let bundle = project.join("dist").join(name);
    let launcher = std::env::current_exe()?;
    let executable = format!("{name}.exe");
    let (yaml, manifest) = preflight(yaml, &launcher, &executable, &args[1])?;
    let outputs = package_outputs(
        &bundle,
        &args[2],
        args.get(3).map(String::as_str),
        manifest.resource_id.is_some(),
    )?;
    run_build_command(
        std::process::Command::new("uv")
            .current_dir(project)
            .args(["sync", "--locked", "--no-dev"]),
        "Install application dependencies",
    )?;
    let python = project.join(".venv/Scripts/python.exe");
    run_build_command(
        std::process::Command::new("uv")
            .current_dir(project)
            .args(["pip", "install", "--python"])
            .arg(&python)
            .arg("-r")
            .arg(&requirements),
        "Install packaging dependencies",
    )?;

    let script = directory.join("build.ps1");
    if script.is_file() {
        // Existing application scripts also copy external resources and isolate DLL lookup.
        run_build_command(
            std::process::Command::new("powershell.exe")
                .current_dir(project)
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(&script),
            "Build PyInstaller application",
        )?;
    } else {
        run_build_command(
            std::process::Command::new(&python)
                .current_dir(project)
                .args([
                    "-I",
                    "-m",
                    "PyInstaller",
                    "--noconfirm",
                    "--clean",
                    "--distpath",
                ])
                .arg(project.join("dist"))
                .arg("--workpath")
                .arg(project.join("build/pyinstaller"))
                .arg(spec),
            "Build PyInstaller spec",
        )?;
    }
    package_body(&bundle, &launcher, outputs, &yaml, &manifest)
}

fn package_outputs(
    bundle: &Path,
    output: &str,
    body: Option<&str>,
    include_full: bool,
) -> Result<(Option<PathBuf>, PathBuf)> {
    let output = std::path::absolute(output)?;
    let body_output = match body {
        Some(path) => std::path::absolute(path)?,
        None => {
            let stem = output
                .file_stem()
                .context("Missing ZIP filename")?
                .to_string_lossy();
            let stem = stem.strip_suffix("-full").unwrap_or(&stem);
            output.with_file_name(format!("{stem}-body.zip"))
        }
    };
    let output = include_full.then_some(output);
    let path_key = |path: &Path| {
        let key = path.to_string_lossy().replace('\\', "/");
        if cfg!(windows) {
            key.to_lowercase()
        } else {
            key
        }
    };
    let bundle_key = path_key(bundle);
    let mut destinations = std::collections::BTreeSet::new();
    // The two final files and their create-new partials must all be distinct.
    for path in output
        .iter()
        .chain(std::iter::once(&body_output))
        .flat_map(|path| [path.clone(), path.with_extension("zip.building")])
    {
        let key = path_key(&path);
        if key == bundle_key || key.starts_with(&(bundle_key.clone() + "/")) || path.exists() {
            bail!("Output ZIPs and partials must be new and outside the clean onedir build");
        }
        if path.file_name().is_none() || !destinations.insert(key) {
            bail!("Body ZIP, Full ZIP and their partials must have different paths");
        }
    }
    for path in output.iter().chain(std::iter::once(&body_output)) {
        fs::create_dir_all(path.parent().context("Missing output directory")?)?;
    }
    Ok((output, body_output))
}

fn run_build_command(command: &mut std::process::Command, description: &str) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("{description}: could not start process"))?;
    if !status.success() {
        bail!("{description} failed: {status}");
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn invalid_publish_inputs_fail_before_any_build_command() {
        let root =
            std::env::temp_dir().join(format!("pyappify-preflight-{:x}", rand::random::<u64>()));
        fs::create_dir_all(root.join("packaging")).unwrap();
        fs::write(root.join("packaging/app.spec"), b"unused build fixture").unwrap();
        fs::write(root.join("packaging/requirements.txt"), b"").unwrap();
        let config = root.join("app.yml");
        for (profiles, resource, version, diagnosis) in [
            (
                "  - name: default\n",
                "example",
                "invalid-version",
                "Invalid frozen package version",
            ),
            (
                "  - name: default\n",
                "invalid resource",
                "v1.4.9",
                "Invalid MirrorChyan resource_id",
            ),
            (" []\n", "example", "v1.4.9", "no profiles"),
        ] {
            fs::write(
                &config,
                format!(
                    "name: example\npackaging: packaging\nmirrorchyan:\n  resource_id: {resource}\nprofiles:\n{profiles}"
                ),
            )
            .unwrap();
            let error = build_and_package(&[
                config.to_string_lossy().into_owned(),
                version.into(),
                root.join("full.zip").to_string_lossy().into_owned(),
            ])
            .unwrap_err();
            assert!(format!("{error:#}").contains(diagnosis), "{error:#}");
            assert!(!root.join(".venv").exists());
            assert!(!root.join("dist").exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn fixture() -> (PathBuf, PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("pyappify-frozen-{:x}", rand::random::<u64>()));
        let bundle = root.join("clean");
        fs::create_dir_all(bundle.join("_internal")).unwrap();
        fs::write(bundle.join("application.exe"), b"frozen application").unwrap();
        fs::write(bundle.join("_internal/python312.dll"), b"python runtime").unwrap();
        let config = root.join("app.yml");
        fs::write(&config, "name: example\npackaging: packaging\nprofiles:\n  - name: default\n    main_script: main.py\n").unwrap();
        (root, bundle, config)
    }

    #[test]
    fn mirror_generates_full_and_body_with_the_original_installation() {
        let (root, bundle, config) = fixture();
        let launcher = root.join("example.exe");
        let full_path = root.join("full.zip");
        let body_path = root.join("body.zip");
        let args = [
            bundle.to_string_lossy().into_owned(),
            launcher.to_string_lossy().into_owned(),
            config.to_string_lossy().into_owned(),
            "application.exe".into(),
            "v1.4.9".into(),
            full_path.to_string_lossy().into_owned(),
            body_path.to_string_lossy().into_owned(),
        ];
        let yaml = fs::read_to_string(&config).unwrap()
            + "mirrorchyan:\n  resource_id: example-resource\n";
        fs::write(&config, &yaml).unwrap();
        fs::write(&launcher, format!("binary prefix\0pyappify.embedded-yaml.v1.begin\0{yaml}\0pyappify.embedded-yaml.v1.end\0binary suffix")).unwrap();
        package(&args).unwrap();
        let mut body = zip::ZipArchive::new(fs::File::open(&body_path).unwrap()).unwrap();
        let mut full = zip::ZipArchive::new(fs::File::open(&full_path).unwrap()).unwrap();
        assert_eq!(full.len(), body.len() + 1);
        assert!(full.by_name("example.exe").is_ok());
        assert!(full.by_name("pyappify.yml").is_err());
        for index in 0..body.len() {
            let mut entry = body.by_index(index).unwrap();
            let mut original = Vec::new();
            let mut packaged = Vec::new();
            let name = format!("data/apps/example/working/{}", entry.name());
            entry.read_to_end(&mut original).unwrap();
            full.by_name(&name)
                .unwrap()
                .read_to_end(&mut packaged)
                .unwrap();
            assert_eq!(original, packaged, "{name}");
        }
        let working = root.join("data/apps/example/working");
        body.extract(&working).unwrap();
        let (record, app) = package::read_package(&root, "example").unwrap().unwrap();
        assert_eq!(app.update_source, crate::mirror::UpdateSource::Mirrorchyan);
        assert_eq!(record.resource_id.as_deref(), Some("example-resource"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn without_mirror_generates_only_body_and_is_not_a_launcher_installation() {
        let (root, bundle, config) = fixture();
        let body_path = root.join("standalone-body.zip");
        package(&[
            bundle.to_string_lossy().into_owned(),
            root.join("absent-launcher.exe")
                .to_string_lossy()
                .into_owned(),
            config.to_string_lossy().into_owned(),
            "application.exe".into(),
            "v1.4.9".into(),
            root.join("full.zip").to_string_lossy().into_owned(),
            body_path.to_string_lossy().into_owned(),
        ])
        .unwrap();
        let mut body = zip::ZipArchive::new(fs::File::open(body_path).unwrap()).unwrap();
        assert!(body.by_name("application.exe").is_ok());
        assert!(body.by_name("pyappify-files.json").is_ok());
        let working = root.join("data/apps/example/working");
        body.extract(&working).unwrap();
        assert!(package::read_package(&root, "example").is_err());
        let yaml: Value =
            serde_yaml::from_str(&fs::read_to_string(working.join("pyappify.yml")).unwrap())
                .unwrap();
        assert!(yaml.get("mirrorchyan").is_none() && yaml.get("update_source").is_none());
        assert!(!root.join("full.zip").exists());
        let (full, body) = package_outputs(
            &bundle,
            root.join("default-full.zip").to_str().unwrap(),
            None,
            false,
        )
        .unwrap();
        assert!(full.is_none());
        assert_eq!(body, root.join("default-body.zip"));
        fs::remove_dir_all(root).unwrap();
    }
}
