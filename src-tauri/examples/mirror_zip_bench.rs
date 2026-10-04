//! Isolated timing against a prepared full ZIP. Never targets an existing install.
use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    time::Instant,
};
use zip::write::SimpleFileOptions;

#[path = "../src/mirror_zip.rs"]
#[allow(dead_code)]
mod mirror_zip;
#[path = "../src/program_files.rs"]
#[allow(dead_code)]
mod program_files;
#[path = "../src/restart_manager.rs"]
#[allow(dead_code)]
mod restart_manager;

#[derive(Serialize)]
struct Measurement {
    scenario: String,
    zip_bytes: u64,
    seconds: f64,
    checksum_seconds: f64,
    prepare_seconds: f64,
    validation_seconds: f64,
    extraction_seconds: f64,
    occupancy_seconds: f64,
    occupancy_files: usize,
    apply_seconds: f64,
    cleanup_seconds: f64,
}

fn measure(
    zip: &Path,
    root: &Path,
    package: &mirror_zip::Package,
    patch: bool,
    scenario: &str,
) -> Result<Measurement> {
    let started = Instant::now();
    let mut input = fs::File::open(zip)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let _hash = digest.finalize();
    let checksum_seconds = started.elapsed().as_secs_f64();
    let preparing = Instant::now();
    let prepared = mirror_zip::prepare(
        zip,
        root,
        &mirror_zip::Expected {
            app_name: &package.app_name,
            resource_id: &package.resource_id,
            version: &package.version,
            launcher: &package.launcher,
        },
        patch,
        || false,
    )?;
    let prepare_seconds = preparing.elapsed().as_secs_f64();
    let validation_seconds = prepared.validation_seconds;
    let extraction_seconds = prepared.extraction_seconds;
    let occupancy_started = Instant::now();
    let files =
        restart_manager::resources_for_replacements(&prepared.replacement_paths()?, patch, || {
            false
        })?;
    let occupancy_files = files.len();
    let session = restart_manager::Session::release(
        &files,
        &restart_manager::Application {
            directory: root.join(package.base()),
            executables: vec![root
                .join(package.base())
                .join("working")
                .join(package.executable())],
            processes: vec![],
        },
        || false,
    )?;
    let occupancy_seconds = occupancy_started.elapsed().as_secs_f64();
    let config_path = root.join(package.base()).join("app.json");
    let previous = fs::read_to_string(&config_path)?;
    let mut config: serde_json::Value = serde_json::from_str(&previous)?;
    config["current_version"] = package.version.clone().into();
    let applying = Instant::now();
    prepared.apply(&serde_json::to_vec_pretty(&config)?, &previous, || false)?;
    session.finish()?;
    let apply_seconds = applying.elapsed().as_secs_f64();
    let mut result = Measurement {
        scenario: scenario.into(),
        zip_bytes: fs::metadata(zip)?.len(),
        seconds: started.elapsed().as_secs_f64(),
        checksum_seconds,
        prepare_seconds,
        validation_seconds,
        extraction_seconds,
        occupancy_seconds,
        occupancy_files,
        apply_seconds,
        cleanup_seconds: 0.0,
    };
    let actual: serde_json::Value = serde_json::from_str(&fs::read_to_string(config_path)?)?;
    if actual["auto_start"] != true
        || actual["update_method"] != "MANUAL_UPDATE"
        || actual["current_profile"] != package.profiles[0]
    {
        bail!("Preferences changed");
    }
    if mirror_zip::read_package(root)?
        .context("Missing package marker")?
        .version
        != package.version
    {
        bail!("Package marker did not advance");
    }
    if fs::read(root.join(&package.launcher))? != b"existing launcher" {
        bail!("Launcher changed");
    }
    // The launcher schedules this after its completion event. Record the work
    // separately so replacement time does not hide the cost of deleting old trees.
    let cleanup_started = Instant::now();
    mirror_zip::cleanup(root)?;
    result.cleanup_seconds = cleanup_started.elapsed().as_secs_f64();
    println!("{}", serde_json::to_string(&result)?);
    Ok(result)
}

fn patch(path: &Path, package: &mirror_zip::Package, changes: &[(&str, &[u8])]) -> Result<()> {
    let mut zip = zip::ZipWriter::new(fs::File::create(path)?);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(6));
    zip.start_file(mirror_zip::PACKAGE_FILE, options)?;
    zip.write_all(&serde_json::to_vec(package)?)?;
    zip.start_file("changes.json", options)?;
    zip.write_all(&serde_json::to_vec(&serde_json::json!({"modified": std::iter::once(mirror_zip::PACKAGE_FILE).chain(changes.iter().map(|(name,_)| *name)).collect::<Vec<_>>()}))?)?;
    for (name, bytes) in changes {
        zip.start_file(*name, options)?;
        zip.write_all(bytes)?;
    }
    zip.finish()?.sync_all()?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        bail!("Usage: mirror_zip_bench <complete.zip> <new-result-directory>");
    }
    let full = std::path::absolute(&args[0])?;
    let directory = std::path::absolute(&args[1])?;
    if directory.exists() {
        bail!("Result directory must not already exist");
    }
    let mut archive = zip::ZipArchive::new(fs::File::open(&full)?)?;
    let package: mirror_zip::Package =
        serde_json::from_reader(archive.by_name(mirror_zip::PACKAGE_FILE)?)?;
    let files = archive.len();
    let internal_files = archive
        .file_names()
        .filter(|name| {
            name.starts_with(&format!("{}/working/_internal/", package.base()))
                && !name.ends_with('/')
        })
        .count();
    let native = archive
        .file_names()
        .find(|name| name.contains("/numpy/") && name.ends_with(".pyd"))
        .context("Expected numpy native module in the real payload")?
        .to_string();
    drop(archive);
    fs::create_dir_all(&directory)?;
    let root = directory.join("install");
    fs::create_dir_all(root.join(package.base()))?;
    fs::write(root.join(&package.launcher), b"existing launcher")?;
    let mut launcher_open = fs::OpenOptions::new();
    launcher_open.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        launcher_open.share_mode(1);
    }
    let _launcher_locked = launcher_open.open(root.join(&package.launcher))?;
    let config = serde_json::json!({"name":package.app_name,"current_version":null,"auto_start":true,"update_method":"MANUAL_UPDATE","current_profile":package.profiles[0]});
    fs::write(
        root.join(package.base()).join("app.json"),
        serde_json::to_vec_pretty(&config)?,
    )?;
    let mut results = Vec::new();
    results.push(measure(
        &full,
        &root,
        &package,
        false,
        "complete ZIP into empty payload directory",
    )?);
    let working = root.join(package.base()).join("working");
    fs::create_dir_all(working.join("configs"))?;
    fs::write(
        working.join("configs/zip-benchmark.json"),
        b"preserve this user data",
    )?;
    let main_name = format!("{}/working/README.md", package.base());
    let requirements_name = format!("{}/working/SPONSOR.md", package.base());
    let main = fs::read(root.join(&main_name))?;
    let requirements = fs::read(root.join(&requirements_name))?;
    let mut open = fs::OpenOptions::new();
    open.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        open.share_mode(1);
    }
    let locked = open.open(root.join(&native))?;
    for number in 1..=2 {
        let mut next = package.clone();
        next.version = format!("v0.0.{number}");
        let mut content = main.clone();
        content.extend_from_slice(format!("\n# ZIP timing revision {number}\n").as_bytes());
        let mut req = requirements.clone();
        req.extend_from_slice(format!("\n# ZIP timing revision {number}\n").as_bytes());
        let path = directory.join(format!("unchanged-lib-{number}.zip"));
        patch(
            &path,
            &next,
            &[(&main_name, &content), (&requirements_name, &req)],
        )?;
        results.push(measure(&path,&root,&next,true,&format!("unchanged lib incremental ZIP {number}; numpy native module locked against write/delete"))?);
        if fs::read(root.join(&main_name))? != content {
            bail!("Changed bundled document was not installed");
        }
    }
    drop(locked);
    // A missing baseline uses the complete package and removes stale libraries.
    fs::remove_file(root.join(package.base()).join(mirror_zip::BASELINE_FILE))?;
    let stale = root
        .join(package.base())
        .join("working/_internal/numpy-old.dist-info");
    fs::create_dir_all(&stale)?;
    fs::write(stale.join("METADATA"), b"Name: numpy\nVersion: 0.0.0\n")?;
    results.push(measure(
        &full,
        &root,
        &package,
        false,
        "complete fallback without baseline with stale numpy metadata",
    )?);
    if stale.exists() {
        bail!("Full package did not remove stale dependency metadata");
    }
    if fs::read(working.join("configs/zip-benchmark.json"))? != b"preserve this user data" {
        bail!("User data changed");
    }
    let mut next = package.clone();
    next.version = "v0.0.3".into();
    let path = directory.join("unchanged-after-full.zip");
    patch(
        &path,
        &next,
        &[(&main_name, &main), (&requirements_name, &requirements)],
    )?;
    results.push(measure(
        &path,
        &root,
        &next,
        true,
        "unchanged lib incremental after complete fallback",
    )?);
    // Report the original package checksum once; no per-file hashes are scanned.
    let report = serde_json::json!({"full_zip":full,"root":root,"archive_entries":files,"internal_files":internal_files,"package":package,"measurements":results,"checks":{"locked_launcher_unchanged":true,"locked_unchanged_numpy":true,"stale_dependency_removed":true,"user_preferences_preserved":true,"user_data_preserved":true}});
    fs::write(
        directory.join("results.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("Report: {}", directory.join("results.json").display());
    Ok(())
}
