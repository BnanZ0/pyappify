//! Build and package PyInstaller applications through the launcher CLI.
use anyhow::{bail, Context, Result};
use serde_yaml::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};
use zip::write::SimpleFileOptions;

use crate::{mirror_zip, program_files::ProgramFiles};

pub fn package(args: &[String]) -> Result<()> {
    if !(7..=8).contains(&args.len()) {
        bail!("Usage: mirror_frozen_pack <clean-onedir> <launcher.exe> <app.yml> <application.exe> <resource-id> <version> <mirror.zip> [body.zip]");
    }
    let (bundle, launcher, config, executable, resource_id, version, output) = (
        &args[0], &args[1], &args[2], &args[3], &args[4], &args[5], &args[6],
    );
    let bundle = std::path::absolute(bundle)?;
    crate::program_files::reject_link(&bundle)?;
    if bundle.file_name().is_some_and(|name| name == "working")
        && bundle
            .parent()
            .is_some_and(|parent| parent.join("app.json").is_file())
    {
        bail!("Package a clean build directory; an installed working directory contains local user files");
    }
    let mut yaml: Value = serde_yaml::from_str(&fs::read_to_string(config)?)?;
    let app_name = yaml
        .get("name")
        .and_then(Value::as_str)
        .context("Missing application name")?
        .to_string();
    let profiles = yaml
        .get("profiles")
        .and_then(Value::as_sequence)
        .context("Missing application profiles")?
        .iter()
        .map(|profile| {
            profile
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .context("Invalid application profile")
        })
        .collect::<Result<Vec<_>>>()?;
    let manifest = mirror_zip::Package {
        format: 2,
        app_name,
        resource_id: resource_id.clone(),
        version: version.clone(),
        launcher: Path::new(launcher)
            .file_name()
            .context("Missing launcher filename")?
            .to_string_lossy()
            .into_owned(),
        runtime: mirror_zip::Runtime::Pyinstaller {
            executable: executable.clone(),
        },
        profiles,
        preserve_paths: Vec::new(),
    };
    manifest.validate(&mirror_zip::Expected {
        app_name: &manifest.app_name,
        resource_id,
        version,
        launcher: &manifest.launcher,
    })?;
    if !bundle.join(executable).is_file() || !bundle.join("_internal").is_dir() {
        bail!("Incomplete onedir: missing application executable or _internal");
    }
    let output = std::path::absolute(output)?;
    let body_output = match args.get(7) {
        Some(path) => std::path::absolute(path)?,
        None => {
            let stem = output
                .file_stem()
                .context("Missing ZIP filename")?
                .to_string_lossy();
            let stem = stem
                .strip_suffix("-mirror-full")
                .or_else(|| stem.strip_suffix("-full"))
                .unwrap_or(&stem);
            output.with_file_name(format!("{stem}-body.zip"))
        }
    };
    for path in [&output, &body_output] {
        if path.starts_with(&bundle) || path.exists() {
            bail!("Output ZIPs must be new and outside the clean onedir build");
        }
        fs::create_dir_all(path.parent().context("Missing output directory")?)?;
    }
    if output == body_output {
        bail!("Body ZIP and Mirror ZIP must have different paths");
    }
    yaml["mirrorchyan"]["resource_id"] = Value::String(resource_id.clone());
    yaml["update_source"] = Value::String("mirrorchyan".into());
    let yaml_bytes = serde_yaml::to_string(&yaml)?;
    // This runs after build.ps1 has copied all external application resources.
    let body_config = bundle.join(mirror_zip::ROOT_CONFIG_FILE);
    if body_config.exists() {
        crate::program_files::reject_link(&body_config)?;
    }
    fs::write(body_config, &yaml_bytes)?;
    let program_files = ProgramFiles::capture_clean_body(&bundle)?;
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
    fs::rename(&body_partial, &body_output)?;

    // Reuse the finished body ZIP, including exactly the same program manifest.
    let partial = output.with_extension("zip.building");
    let mut archive = zip::ZipWriter::new(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?,
    );
    archive.start_file(mirror_zip::PACKAGE_FILE, options)?;
    archive.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    archive.start_file(&manifest.launcher, options)?;
    std::io::copy(&mut fs::File::open(launcher)?, &mut archive)?;
    archive.start_file(mirror_zip::ROOT_CONFIG_FILE, options)?;
    archive.write_all(yaml_bytes.as_bytes())?;
    let files = program_files.files.len() as u64 + 3;
    let mut body = zip::ZipArchive::new(fs::File::open(&body_output)?)?;
    for index in 0..body.len() {
        let entry = body.by_index(index)?;
        let name = format!("{working}/{}", entry.name());
        archive.raw_copy_file_rename(entry, name)?;
    }
    archive.finish()?.sync_all()?;
    // Read back metadata and required entries before making the final ZIP visible.
    let mut check = zip::ZipArchive::new(fs::File::open(&partial)?)?;
    let mut metadata = String::new();
    check
        .by_name(mirror_zip::PACKAGE_FILE)?
        .read_to_string(&mut metadata)?;
    let written: mirror_zip::Package = serde_json::from_str(&metadata)?;
    if written != manifest || check.by_name(&format!("{working}/{executable}")).is_err() {
        bail!("Incomplete output ZIP");
    }
    drop(check);
    if output.exists() {
        bail!("Output ZIP already exists");
    }
    fs::rename(&partial, &output)?;
    let mut digest = Sha256::new();
    let mut input = fs::File::open(&output)?;
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
        serde_json::json!({"zip": output, "body_zip": body_output, "body_files": program_files.files.len(), "bytes": fs::metadata(&output)?.len(), "files": files, "sha256": format!("{:x}", digest.finalize()), "package": manifest})
    );
    Ok(())
}

/// Run before Tauri initialization: packaging must not open the launcher UI.
pub fn try_run(args: &[String]) -> Option<Result<()>> {
    if matches!(args.get(1).map(String::as_str), Some("-c" | "--command"))
        && args.get(2).map(String::as_str) == Some("mirror-zip")
    {
        Some(build_and_package(&args[3..]))
    } else {
        None
    }
}

fn build_and_package(args: &[String]) -> Result<()> {
    if !(4..=5).contains(&args.len()) {
        bail!("Usage: pyappify -c mirror-zip <app.yml> <resource-id> <version> <mirror.zip> [body.zip]");
    }
    if !cfg!(windows) {
        bail!("Mirror PyInstaller builds currently require Windows");
    }
    let config = std::path::absolute(&args[0])?;
    let project = config
        .parent()
        .context("Application config has no parent directory")?;
    let yaml: Value = serde_yaml::from_str(&fs::read_to_string(&config)?)?;
    let packaging = yaml
        .get("mirrorchyan")
        .and_then(|mirror| mirror.get("packaging"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context(
            "Set mirrorchyan.packaging to the application's packaging directory in pyappify.yml",
        )?;
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
    let output = std::path::absolute(&args[3])?;
    if output.starts_with(&bundle) || output.exists() {
        bail!("Output ZIP must be new and outside the onedir build");
    }
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
    let mut package_args = vec![
        bundle.to_string_lossy().into_owned(),
        std::env::current_exe()?.to_string_lossy().into_owned(),
        config.to_string_lossy().into_owned(),
        format!("{name}.exe"),
        args[1].clone(),
        args[2].clone(),
        output.to_string_lossy().into_owned(),
    ];
    if let Some(body) = args.get(4) {
        package_args.push(body.clone());
    }
    package(&package_args)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_clean_body_produces_two_zips_with_identical_body_and_manifest() {
        let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/package-body-tests")
            .join(format!("{:x}", rand::random::<u64>()));
        let body = directory.join("body");
        for name in [
            "sample.exe",
            "_internal/python312.dll",
            "assets/model.dat",
            "other/unlisted.resource",
        ] {
            let path = body.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("program: {name}")).unwrap();
        }
        let launcher = directory.join("launcher.exe");
        fs::write(&launcher, b"launcher").unwrap();
        let config = directory.join("app.yml");
        fs::write(
            &config,
            "name: sample\nprofiles:\n  - name: default\nmirrorchyan:\n  resource_id: local\n",
        )
        .unwrap();
        let mirror = directory.join("sample-win-x86_64-v1-full.zip");
        package(&[
            body.to_string_lossy().into_owned(),
            launcher.to_string_lossy().into_owned(),
            config.to_string_lossy().into_owned(),
            "sample.exe".into(),
            "local".into(),
            "v1".into(),
            mirror.to_string_lossy().into_owned(),
        ])
        .unwrap();
        let mut body_zip = zip::ZipArchive::new(
            fs::File::open(directory.join("sample-win-x86_64-v1-body.zip")).unwrap(),
        )
        .unwrap();
        let mut mirror_zip = zip::ZipArchive::new(fs::File::open(&mirror).unwrap()).unwrap();
        assert!(body_zip.by_name("sample.exe").is_ok());
        assert!(body_zip.by_name("other/unlisted.resource").is_ok());
        assert!(body_zip.by_name("launcher.exe").is_err());
        assert!(body_zip.by_name(crate::mirror_zip::PACKAGE_FILE).is_err());
        assert_eq!(body_zip.len() + 3, mirror_zip.len());
        for index in 0..body_zip.len() {
            let mut entry = body_zip.by_index(index).unwrap();
            let mut expected = Vec::new();
            entry.read_to_end(&mut expected).unwrap();
            let mut actual = Vec::new();
            mirror_zip
                .by_name(&format!("data/apps/sample/working/{}", entry.name()))
                .unwrap()
                .read_to_end(&mut actual)
                .unwrap();
            assert_eq!(actual, expected);
        }
        drop(body_zip);
        drop(mirror_zip);
        fs::remove_dir_all(directory).unwrap();
    }
}
