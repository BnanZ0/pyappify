//! Validate and stage Mirror ZIPs before applying a durable file transaction.
use crate::extensions::file_operations::check_cancelled;
use crate::extensions::install_transaction::{self as transaction, Action, TASK_DIR};
use crate::frozen::package::*;
use crate::frozen::program_files::ProgramFiles;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufWriter, Read, Write},
    path::Path,
};
use transaction::InstallTransaction;

#[derive(Default, Deserialize)]
struct Changes {
    #[serde(default)]
    added: Vec<String>,
    #[serde(default)]
    modified: Vec<String>,
    #[serde(default)]
    deleted: Vec<String>,
    #[serde(default)]
    added_dir: Vec<String>,
    #[serde(default)]
    deleted_dir: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("A complete ZIP is required: {0}")]
pub struct NeedsFullPackage(pub String);

/// ZIP scope checks and Windows destinations use the same case-insensitive identity.
fn working_path<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    // Case folding can change a component's byte length (e.g. İ and i + ◌̇).
    let mut components = name.splitn(prefix.matches('/').count() + 1, '/');
    for expected in prefix.split_terminator('/') {
        if key(components.next()?) != key(expected) {
            return None;
        }
    }
    components.next()
}

pub struct Prepared {
    pub package: Package,
    pub working_config: crate::app::App,
    pub incremental: bool,
    pub validation_seconds: f64,
    pub extraction_seconds: f64,
    transaction: InstallTransaction,
}

impl std::ops::Deref for Prepared {
    type Target = InstallTransaction;
    fn deref(&self) -> &Self::Target {
        &self.transaction
    }
}
impl std::ops::DerefMut for Prepared {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.transaction
    }
}

/// Back up working before downloading a complete Mirror installation.
pub(crate) fn plan_installation(
    root: &Path,
    app_name: &str,
) -> Result<(InstallTransaction, Option<String>)> {
    let task = safe_join(root, TASK_DIR)?;
    if task.exists() {
        bail!("An unfinished installation must be recovered before installing");
    }
    let mut transaction = InstallTransaction::new(root, app_name)?;
    let base = transaction.base();
    let backup = transaction.backup(&format!("{base}/working"))?;
    fs::create_dir_all(task.join("staged"))?;
    Ok((transaction, backup))
}

/// Inspect a downloaded response before choosing the incremental or full route.
pub(crate) fn is_incremental(archive_path: &Path) -> Result<bool> {
    let mut archive = zip::ZipArchive::new(fs::File::open(archive_path)?)?;
    let incremental = match archive.by_name("changes.json") {
        Ok(_) => Ok(true),
        Err(zip::result::ZipError::FileNotFound) => Ok(false),
        Err(error) => Err(error.into()),
    };
    incremental
}

pub(crate) fn prepare_incremental(
    archive_path: &Path,
    root: &Path,
    expected: &Expected<'_>,
    cancelled: impl Fn() -> bool + Sync,
) -> Result<Prepared> {
    prepare(archive_path, root, expected, None, None, cancelled)
}

pub(crate) fn stage_full(
    archive_path: &Path,
    root: &Path,
    expected: &Expected<'_>,
    transaction: InstallTransaction,
    user_backup: Option<(String, ProgramFiles)>,
    cancelled: impl Fn() -> bool + Sync,
) -> Result<Prepared> {
    prepare(
        archive_path,
        root,
        expected,
        Some(transaction),
        user_backup,
        cancelled,
    )
}

fn prepare(
    archive_path: &Path,
    root: &Path,
    expected: &Expected<'_>,
    preparation: Option<InstallTransaction>,
    user_backup: Option<(String, ProgramFiles)>,
    cancelled: impl Fn() -> bool + Sync,
) -> Result<Prepared> {
    let validation_started = std::time::Instant::now();
    check_cancelled(&cancelled)?;
    let installation_prepared = preparation.is_some();
    if let Some(transaction) = &preparation {
        if transaction.root != root
            || transaction.app_name != expected.app_name
            || !transaction::replacement_pending(root)
        {
            bail!("The prepared installation does not match this full ZIP operation");
        }
    }
    let mut archive = zip::ZipArchive::new(fs::File::open(archive_path)?)
        .context("The update is not a valid ZIP package")?;
    let changes = match archive.by_name("changes.json") {
        Ok(file) => Some(read_json::<Changes>(file, 16 * 1024 * 1024)?),
        Err(zip::result::ZipError::FileNotFound) => None,
        Err(error) => return Err(error.into()),
    };
    let (package, working_config, new_program) = read_zip_package(
        &mut archive,
        expected.app_name,
        changes.as_ref().map(|_| root),
    )?;
    package.validate(expected)?;
    if changes.is_some() && installation_prepared {
        return Err(
            NeedsFullPackage("the installed directory has no trusted ZIP baseline".into()).into(),
        );
    }
    if changes.is_none() && !installation_prepared {
        return Err(NeedsFullPackage("the server returned a complete ZIP".into()).into());
    }
    let mut entries = BTreeMap::new();
    let mut names = BTreeSet::new();
    let mut spellings = BTreeMap::new();
    let mut extraction_total = 0u64;
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index)?;
        let name = entry.name().trim_end_matches('/').to_string();
        relative(&name)?;
        if !names.insert(key(&name)) {
            bail!("ZIP contains duplicate paths: {name}");
        }
        if entry.is_symlink() || entry.encrypted() {
            bail!("ZIP links and encrypted entries are unsupported");
        }
        if name == "changes.json" {
            if entry.is_dir() {
                bail!("Invalid changes.json entry");
            }
            continue;
        }
        if !package.allows(&name, entry.is_dir()) {
            bail!("ZIP contains an unsupported or preserved path: {name}");
        }
        if package.ignores_launcher(&name) {
            // Keep the running launcher; the skipped bytes are not inspected.
            continue;
        }
        if !entry.is_dir() {
            extraction_total = extraction_total
                .checked_add(entry.size())
                .context("ZIP uncompressed size overflow")?;
        }
        if entries
            .insert(key(&name), (index, name.clone(), entry.is_dir()))
            .is_some()
        {
            bail!("ZIP contains duplicate paths: {name}");
        }
        // Also reject case aliases in implicit parents (e.g. Src/a and src/b).
        let mut parent = String::new();
        for component in name.split('/') {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(component);
            if let Some(previous) = spellings.insert(key(&parent), parent.clone()) {
                if previous != parent {
                    bail!("ZIP contains inconsistent path casing: {name}");
                }
            }
        }
    }
    for (_, name, _) in entries.values() {
        let mut parent = Path::new(name).parent();
        while let Some(path) = parent.filter(|p| !p.as_os_str().is_empty()) {
            let name = path.to_string_lossy().replace('\\', "/");
            if entries.get(&key(&name)).is_some_and(|(_, _, dir)| !dir) {
                bail!("ZIP contains a file used as a parent directory: {name}");
            }
            parent = path.parent();
        }
    }
    let base = package.base();
    let working_prefix = format!("{base}/working/");
    let old_program = if changes.is_some() {
        Some(
            ProgramFiles::read(&root.join(&base).join("working")).map_err(|error| {
                NeedsFullPackage(format!("missing or invalid program manifest: {error:#}"))
            })?,
        )
    } else {
        None
    };
    let old_program_keys = old_program
        .as_ref()
        .map(ProgramFiles::keys)
        .unwrap_or_default();
    let mut deleted = Vec::new();
    let mut added_dirs = Vec::new();
    if let Some(changes) = &changes {
        let mut writes = BTreeSet::new();
        for (paths, added) in [(&changes.added, true), (&changes.modified, false)] {
            for name in paths {
                relative(name)?;
                if package.ignores_launcher(name) {
                    continue;
                }
                if !package.allows(name, false) || !writes.insert(key(name)) {
                    bail!("Invalid or duplicate changes.json file: {name}");
                }
                if !entries
                    .get(&key(name))
                    .is_some_and(|(_, stored, dir)| stored == name && !dir)
                {
                    bail!("changes.json file is missing from ZIP: {name}");
                }
                let destination = safe_join(root, name)?;
                let directory_to_file = added
                    && changes.deleted_dir.iter().any(|dir| dir == name)
                    && destination.is_dir();
                if destination.is_file()
                    && working_path(name, &working_prefix)
                        .is_some_and(|relative| !old_program_keys.contains(&key(relative)))
                {
                    bail!("ZIP cannot overwrite an untracked user file: {name}");
                }
                if directory_to_file {
                    for entry in walkdir::WalkDir::new(&destination).follow_links(false) {
                        check_cancelled(&cancelled)?;
                        let entry = entry?;
                        crate::frozen::program_files::reject_link(entry.path())?;
                        if entry.file_type().is_file() {
                            let name = entry
                                .path()
                                .strip_prefix(root)?
                                .to_string_lossy()
                                .replace('\\', "/");
                            let relative = working_path(&name, &working_prefix)
                                .context("ZIP directory is outside the application body")?;
                            if !old_program_keys.contains(&key(&relative)) {
                                bail!("ZIP cannot replace a directory containing user file: {relative}");
                            }
                        }
                    }
                }
                // ZIP replacements contain the entire new file, so a missing old file
                // can be recreated. An existing path must still match its declared type.
                if !directory_to_file && destination.exists() && (added || !destination.is_file()) {
                    return Err(NeedsFullPackage(format!("baseline file differs: {name}")).into());
                }
                if !added && !destination.exists() {
                    for parent in destination.ancestors().skip(1) {
                        if parent.exists() {
                            if !parent.is_dir() {
                                return Err(NeedsFullPackage(format!(
                                    "baseline parent differs: {name}"
                                ))
                                .into());
                            }
                            break;
                        }
                    }
                }
            }
        }
        for (_, name, directory) in entries.values() {
            if !directory && !writes.contains(&key(name)) {
                bail!("ZIP file is not listed in changes.json: {name}");
            }
        }
        let mut deletions = BTreeSet::new();
        for (paths, directory) in [(&changes.deleted, false), (&changes.deleted_dir, true)] {
            for name in paths {
                relative(name)?;
                if !directory && package.ignores_launcher(name) {
                    continue;
                }
                if !package.allows(name, directory)
                    || ["data", "data/apps", &base, &format!("{base}/working")]
                        .iter()
                        .any(|root| root.eq_ignore_ascii_case(name))
                    || within(&format!("{base}/working/{}", package.executable()), name)
                    || within(&format!("{base}/working/_internal"), name)
                    || (writes.contains(&key(name)) && !(directory && changes.added.contains(name)))
                    || !deletions.insert(key(name))
                {
                    bail!("Invalid changes.json deletion: {name}");
                }
                let target = safe_join(root, name)?;
                // A missing target is already deleted, regardless of manifest ownership.
                if target.exists() && (!directory || target.is_file()) {
                    if let Some(relative) = working_path(name, &working_prefix) {
                        if !old_program_keys.contains(&key(relative)) {
                            bail!("ZIP cannot delete an untracked user file: {name}");
                        }
                    }
                }
                let file_to_directory = !directory && changes.added_dir.contains(name);
                let directory_to_file = directory && changes.added.contains(name);
                if entries.values().any(|(_, stored, dir)| {
                    within(stored, name)
                        && !(file_to_directory || (directory_to_file && stored == name && !dir))
                }) {
                    bail!("ZIP writes inside a deleted path: {name}");
                }
                deleted.push(name.clone());
            }
        }
        // A deleted directory already owns its listed descendants.
        deleted.retain(|name| {
            !changes
                .deleted_dir
                .iter()
                .any(|dir| dir != name && within(name, dir))
        });
        for name in &changes.added_dir {
            relative(name)?;
            if !package.allows(name, true)
                || changes.deleted_dir.iter().any(|dir| within(name, dir))
            {
                bail!("Invalid changes.json directory: {name}");
            }
            safe_join(root, name)?;
            added_dirs.push(name.clone());
        }
    } else {
        let required = [
            format!("{base}/working/{WORKING_CONFIG_FILE}"),
            format!("{base}/working/{}", package.executable()),
        ];
        if !entries.values().any(|(_, name, dir)| {
            !dir && within(name, &format!("{base}/working/_internal"))
                && !name.eq_ignore_ascii_case(&format!("{base}/working/_internal"))
        }) {
            bail!("Incomplete PyInstaller ZIP: missing _internal runtime files");
        }
        for required in required {
            if !entries.get(&key(&required)).is_some_and(|(_, _, dir)| !dir) {
                bail!("Incomplete application ZIP: missing {required}");
            }
        }
    }
    let task = safe_join(root, TASK_DIR)?;
    if task.exists() && !installation_prepared {
        bail!("An unfinished ZIP transaction must be recovered before updating");
    }
    fs::create_dir_all(task.join("staged"))?;
    let validation_seconds = validation_started.elapsed().as_secs_f64();
    let extraction_started = std::time::Instant::now();
    let mut extracted = 0u64;
    let mut last_emit = extraction_started;
    super::download::progress(&package.app_name, "extracting", 0, Some(extraction_total));
    let extraction = (|| -> Result<()> {
        let mut made_dirs = BTreeSet::new();
        let mut buffer = vec![0u8; 256 * 1024];
        for (index, name, directory) in entries.values() {
            check_cancelled(&cancelled)?;
            let destination = task.join("staged").join(relative(name)?);
            if *directory {
                if made_dirs.insert(destination.clone()) {
                    fs::create_dir_all(destination)?;
                }
                continue;
            }
            let parent = destination.parent().unwrap();
            if made_dirs.insert(parent.to_path_buf()) {
                fs::create_dir_all(parent)?;
            }
            let mut input = archive.by_index(*index)?;
            let mut output = BufWriter::with_capacity(256 * 1024, fs::File::create(destination)?);
            loop {
                check_cancelled(&cancelled)?;
                let count = input.read(&mut buffer).context("Corrupt ZIP entry")?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count])?;
                extracted += count as u64;
                if extracted < extraction_total && last_emit.elapsed().as_millis() >= 200 {
                    super::download::progress(
                        &package.app_name,
                        "extracting",
                        extracted,
                        Some(extraction_total),
                    );
                    last_emit = std::time::Instant::now();
                }
            }
            output.flush()?;
        }
        Ok(())
    })();
    if let Err(error) = extraction {
        if !installation_prepared {
            let _ = crate::extensions::file_operations::remove_tree(
                &task,
                crate::extensions::file_operations::DeletePolicy::PreserveAttributes,
                "Mirror extraction failure staging cleanup",
                &|| false,
            );
        }
        return Err(error);
    }
    super::download::progress(
        &package.app_name,
        "extracting",
        extracted,
        Some(extraction_total),
    );
    if changes.is_none() && !package.payload_present(&task.join("staged"))? {
        if !installation_prepared {
            let _ = crate::extensions::file_operations::remove_tree(
                &task,
                crate::extensions::file_operations::DeletePolicy::PreserveAttributes,
                "Mirror incomplete package staging cleanup",
                &|| false,
            );
        }
        bail!("Incomplete PyInstaller ZIP: executable, configuration or Python DLL is missing");
    }
    let staged_body = task.join("staged").join(&base).join("working");
    let mut expected_files = if let Some(old) = &old_program {
        old.keys()
    } else {
        BTreeSet::new()
    };
    if let Some(changes) = &changes {
        for name in changes.deleted.iter().chain(&changes.deleted_dir) {
            if let Some(path) = working_path(name, &working_prefix) {
                expected_files.retain(|name| !within(name, path));
            }
        }
    }
    for (_, name, directory) in entries.values() {
        if !directory {
            if let Some(path) = working_path(name, &working_prefix) {
                expected_files.insert(key(path));
            }
        }
    }
    if new_program.keys() != expected_files {
        bail!("The application program manifest does not match the ZIP files and changes");
    }
    if changes.is_some() {
        // Inspect the projected body without copying unchanged files into staging.
        let available = |file: &str| -> Result<bool> {
            if !expected_files.contains(&key(file)) {
                return Ok(false);
            }
            let path = format!("{working_prefix}{file}");
            Ok(if entries.contains_key(&key(&path)) {
                safe_join(&task.join("staged"), &path)?.is_file()
            } else {
                safe_join(root, &path)?.is_file()
            })
        };
        for file in &new_program.files {
            if !available(file)? {
                transaction::recover(root)?;
                return Err(NeedsFullPackage(format!(
                    "program file is missing from the resulting body: {file}"
                ))
                .into());
            }
        }
        let mut runtime = false;
        for file in &new_program.files {
            let name = key(file);
            if name.starts_with("_internal/python")
                && name.ends_with(".dll")
                && name.matches('/').count() == 1
                && available(file)?
            {
                runtime = true;
                break;
            }
        }
        if !runtime
            || !available(WORKING_CONFIG_FILE)?
            || !available(new_program.executable.as_str())?
        {
            transaction::recover(root)?;
            return Err(NeedsFullPackage(
                "executable, configuration or Python DLL is missing from the resulting body".into(),
            )
            .into());
        }
    }
    let mut prepared = Prepared {
        transaction: match preparation {
            Some(transaction) => transaction,
            None => InstallTransaction::new(root, &package.app_name)?,
        },
        package,
        working_config,
        incremental: changes.is_some(),
        validation_seconds,
        extraction_seconds: extraction_started.elapsed().as_secs_f64(),
    };
    if prepared.incremental {
        for name in deleted {
            // Removing a program directory must leave untracked user files inside it.
            let directory_to_file = changes.as_ref().is_some_and(|c| c.added.contains(&name));
            if safe_join(root, &name)?.is_dir() && !directory_to_file {
                if let Some(path) = working_path(&name, &working_prefix) {
                    for file in &old_program.as_ref().unwrap().files {
                        if within(file, path) {
                            let name = format!("{working_prefix}{file}");
                            // A former program file may now be a user directory.
                            // Only move files; empty-dir cleanup preserves its contents.
                            if safe_join(root, &name)?.is_file() {
                                prepared.backup(&name)?;
                            }
                        }
                    }
                    prepared.actions.push(Action::RemoveEmptyDir { path: name });
                }
            } else {
                prepared.backup(&name)?;
            }
        }
        for name in added_dirs {
            prepared.mkdir(&name)?;
        }
        for (_, name, directory) in entries.values() {
            if !directory {
                prepared.replace(name)?;
            }
        }
    } else {
        if let Some((backup, old)) = user_backup {
            crate::frozen::program_files::migrate_users(
                &safe_join(root, &backup)?,
                &staged_body,
                &old,
                &new_program,
                &cancelled,
            )?;
        }
        prepared.backup(&format!("{base}/python"))?;
        prepared.backup(&format!("{base}/repo"))?;
        // This move owns the whole new tree during rollback, including migrated
        // user files. No empty live working directory is created beforehand.
        prepared.replace(&format!("{base}/working"))?;
    }
    Ok(prepared)
}

impl Prepared {
    /// Config and the body's version manifest share the payload rollback journal.
    pub fn apply(
        mut self,
        config: &[u8],
        rollback_config: &str,
        cancelled: impl Fn() -> bool + Sync,
    ) -> Result<()> {
        // Keep delta registration unrestricted, including directory-to-file
        // replacements. Persist the scope for rollback and interrupted recovery.
        if self.incremental {
            self.transaction.preserve_all_file_occupancy();
        }
        self.transaction.apply(config, rollback_config, cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frozen::program_files::MANIFEST_FILE;
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("pyappify-body-zip-{:x}", rand::random::<u64>()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn zip(&self, declared_app: &str, launcher: bool) -> PathBuf {
            let base = format!("data/apps/{declared_app}");
            let mut manifest = ProgramFiles::new(
                [
                    "application.exe",
                    "Ä.dat",
                    "_internal/python312.dll",
                    WORKING_CONFIG_FILE,
                    MANIFEST_FILE,
                ]
                .map(String::from),
            )
            .unwrap();
            manifest.version = "v1.4.9".into();
            manifest.executable = "application.exe".into();
            let path = self.0.join("body.zip");
            let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            if launcher {
                zip.start_file("distribution.exe", options).unwrap();
                zip.write_all(b"uninspected launcher bytes").unwrap();
            }
            for (name, bytes) in [
                (
                    format!("{}/working/application.exe", base),
                    b"fixture application".to_vec(),
                ),
                (format!("{}/working/Ä.dat", base), b"non-ASCII program file".to_vec()),
                (
                    format!("{}/working/_internal/python312.dll", base),
                    b"fixture runtime".to_vec(),
                ),
                (
                    format!("{}/working/{WORKING_CONFIG_FILE}", base),
                    format!("name: {declared_app}\nmirrorchyan:\n  resource_id: example\nprofiles:\n  - name: default\n    main_script: main.py\n").into_bytes(),
                ),
                (
                    format!("{}/working/{MANIFEST_FILE}", base),
                    serde_json::to_vec(&manifest).unwrap(),
                ),
            ] {
                zip.start_file(name, options).unwrap();
                zip.write_all(&bytes).unwrap();
            }
            zip.finish().unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn expected() -> Expected<'static> {
        Expected {
            app_name: "example",
            resource_id: "example",
            version: "v1.4.9",
        }
    }

    #[test]
    fn stages_full_body_with_an_optional_launcher_and_a_renamed_current_launcher() {
        for launcher in [true, false] {
            let fixture = Fixture::new();
            let archive = fixture.zip("example", launcher);
            let root = fixture.0.join("installation");
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("renamed.exe"), b"current launcher").unwrap();
            let (mut transaction, _) = plan_installation(&root, "example").unwrap();
            // Offline staging only: dummy callback, no real installation or RM.
            transaction.apply_preparation("{}", || false).unwrap();
            let prepared =
                stage_full(&archive, &root, &expected(), transaction, None, || false).unwrap();
            assert_eq!(prepared.package.app_name, "example");
            assert_eq!(prepared.package.version, "v1.4.9");
            assert_eq!(prepared.working_config.name, "example");
            assert_eq!(
                fs::read(
                    root.join(TASK_DIR)
                        .join("staged/data/apps/example/working/Ä.dat")
                )
                .unwrap(),
                b"non-ASCII program file"
            );
            assert_eq!(
                fs::read(root.join("renamed.exe")).unwrap(),
                b"current launcher"
            );
            assert!(!root.join(TASK_DIR).join("staged/distribution.exe").exists());
        }
    }

    #[test]
    fn skipping_the_zip_launcher_still_rejects_foreign_package_identity() {
        let fixture = Fixture::new();
        let archive = fixture.zip("other", true);
        let root = fixture.0.join("installation");
        let (mut transaction, _) = plan_installation(&root, "example").unwrap();
        transaction.apply_preparation("{}", || false).unwrap();
        stage_full(&archive, &root, &expected(), transaction, None, || false)
            .err()
            .unwrap();
        assert!(!root.join(TASK_DIR).join("staged/data/apps/other").exists());
    }

    impl Fixture {
        fn rewrite_zip(&self, path: &Path, rewrite: impl FnOnce(&mut Vec<(String, Vec<u8>)>)) {
            let mut entries = {
                let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
                (0..archive.len())
                    .map(|index| {
                        let mut entry = archive.by_index(index).unwrap();
                        let name = entry.name().to_string();
                        let mut bytes = Vec::new();
                        entry.read_to_end(&mut bytes).unwrap();
                        (name, bytes)
                    })
                    .collect()
            };
            rewrite(&mut entries);
            let mut archive = zip::ZipWriter::new(fs::File::create(path).unwrap());
            for (name, bytes) in entries {
                archive
                    .start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                archive.write_all(&bytes).unwrap();
            }
            archive.finish().unwrap();
        }

        fn delta(&self) -> (PathBuf, PathBuf) {
            self.delta_deleting(&[], &[])
        }

        fn delta_deleting(&self, deleted: &[&str], deleted_dirs: &[&str]) -> (PathBuf, PathBuf) {
            self.delta_with_replacements(deleted, deleted_dirs, &[])
        }

        fn delta_with_replacements(
            &self,
            deleted: &[&str],
            deleted_dirs: &[&str],
            replacements: &[(&str, &[u8])],
        ) -> (PathBuf, PathBuf) {
            let full = self.zip("example", true);
            let root = self.0.join("installed");
            let mut archive = zip::ZipArchive::new(fs::File::open(full).unwrap()).unwrap();
            // This ZIP was generated by the fixture, with synthetic files only.
            archive.extract(&root).unwrap();
            let working = root.join("data/apps/example/working");
            let mut manifest = ProgramFiles::read(&working).unwrap();
            for (path, _) in replacements {
                let target = working.join(path);
                fs::create_dir_all(target.parent().unwrap()).unwrap();
                fs::write(target, b"old fixture file").unwrap();
                manifest.files.push((*path).into());
            }
            manifest.version = "v1.4.8".into();
            fs::write(
                working.join(MANIFEST_FILE),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            fs::write(root.join("data/apps/example/app.json"), b"{}").unwrap();
            manifest.version = "v1.4.9".into();
            manifest.files.retain(|file| {
                !deleted
                    .iter()
                    .chain(deleted_dirs)
                    .any(|path| within(file, path))
            });
            let working_prefix = "data/apps/example/working/";
            let manifest_path = format!("data/apps/example/working/{MANIFEST_FILE}");
            let application_path = "data/apps/example/working/application.exe";
            let modified: Vec<_> = [application_path.to_string(), manifest_path.clone()]
                .into_iter()
                .chain(
                    replacements
                        .iter()
                        .map(|(path, _)| format!("{working_prefix}{path}")),
                )
                .collect();
            let delta = self.0.join("delta.zip");
            let mut zip = zip::ZipWriter::new(fs::File::create(&delta).unwrap());
            // The server omits unchanged release metadata from this delta.
            for (name, bytes) in [
                (
                    "changes.json".to_string(),
                    serde_json::to_vec(&serde_json::json!({
                        "modified": modified,
                        "deleted": deleted.iter().map(|path| format!("{working_prefix}{path}")).collect::<Vec<_>>(),
                        "deleted_dir": deleted_dirs.iter().map(|path| format!("{working_prefix}{path}")).collect::<Vec<_>>()
                    }))
                    .unwrap(),
                ),
                (
                    application_path.to_string(),
                    b"updated fixture application".to_vec(),
                ),
                (manifest_path, serde_json::to_vec(&manifest).unwrap()),
            ] {
                zip.start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(&bytes).unwrap();
            }
            for (path, bytes) in replacements {
                zip.start_file(
                    format!("{working_prefix}{path}"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
            (root, delta)
        }
    }

    #[test]
    fn delta_deleting_missing_files_and_directories_commits_the_update() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta_deleting(
            &["ok-nte.pkg", "missing/old.pkg", "retired/old.pkg", "Ä.dat"],
            &["retired", "missing/empty"],
        );
        let working = root.join("data/apps/example/working");
        // A tracked file and untracked paths have already disappeared locally.
        fs::remove_file(working.join("Ä.dat")).unwrap();
        let prepared = prepare_incremental(&delta, &root, &expected(), || false).unwrap();
        prepared
            .apply(b"{\"version\":\"v1.4.9\"}", "{}", || false)
            .unwrap();
        let manifest = ProgramFiles::read(&working).unwrap();
        assert_eq!(manifest.version, "v1.4.9");
        assert!(!manifest.keys().contains(&key("Ä.dat")));
        assert_eq!(
            fs::read(working.join("application.exe")).unwrap(),
            b"updated fixture application"
        );
        for path in ["ok-nte.pkg", "missing", "retired", "Ä.dat"] {
            assert!(!working.join(path).exists());
        }
    }

    #[test]
    fn delta_cannot_delete_an_existing_user_file_even_if_listed_as_a_directory() {
        for directory in [false, true] {
            let fixture = Fixture::new();
            let (root, delta) = if directory {
                fixture.delta_deleting(&[], &["ok-nte.pkg"])
            } else {
                fixture.delta_deleting(&["ok-nte.pkg"], &[])
            };
            let working = root.join("data/apps/example/working");
            fs::write(working.join("ok-nte.pkg"), b"user data").unwrap();
            let error = prepare_incremental(&delta, &root, &expected(), || false)
                .err()
                .unwrap();
            assert!(error
                .to_string()
                .contains("cannot delete an untracked user file"));
            assert_eq!(fs::read(working.join("ok-nte.pkg")).unwrap(), b"user data");
            assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.8");
            assert!(!root.join(TASK_DIR).exists());
        }
    }

    #[test]
    fn delta_deletes_an_existing_tracked_program_file() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta_deleting(&["Ä.dat"], &[]);
        let working = root.join("data/apps/example/working");
        assert!(working.join("Ä.dat").is_file());
        let prepared = prepare_incremental(&delta, &root, &expected(), || false).unwrap();
        prepared
            .apply(b"{\"version\":\"v1.4.9\"}", "{}", || false)
            .unwrap();
        assert!(!working.join("Ä.dat").exists());
        assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.9");
    }

    #[test]
    fn delta_recreates_a_missing_replacement_target() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta();
        let working = root.join("data/apps/example/working");
        fs::remove_file(working.join("application.exe")).unwrap();
        let prepared = prepare_incremental(&delta, &root, &expected(), || false).unwrap();
        prepared
            .apply(b"{\"version\":\"v1.4.9\"}", "{}", || false)
            .unwrap();
        assert_eq!(
            fs::read(working.join("application.exe")).unwrap(),
            b"updated fixture application"
        );
        assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.9");
    }

    #[test]
    fn delta_recreates_missing_replacement_parents_but_preserves_blocking_user_files() {
        for blocking_file in [false, true] {
            let fixture = Fixture::new();
            let (root, delta) = fixture.delta_with_replacements(
                &[],
                &[],
                &[("nested/program/body.dat", b"updated nested file")],
            );
            let working = root.join("data/apps/example/working");
            fs::remove_dir_all(working.join("nested")).unwrap();
            if blocking_file {
                fs::write(working.join("nested"), b"user data").unwrap();
                let error = prepare_incremental(&delta, &root, &expected(), || false)
                    .err()
                    .unwrap();
                assert!(error.is::<NeedsFullPackage>());
                assert_eq!(fs::read(working.join("nested")).unwrap(), b"user data");
                assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.8");
                assert!(!root.join(TASK_DIR).exists());
            } else {
                let prepared = prepare_incremental(&delta, &root, &expected(), || false).unwrap();
                prepared
                    .apply(b"{\"version\":\"v1.4.9\"}", "{}", || false)
                    .unwrap();
                assert_eq!(
                    fs::read(working.join("nested/program/body.dat")).unwrap(),
                    b"updated nested file"
                );
                assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.9");
            }
        }
    }

    #[test]
    fn cancelled_delta_restores_a_missing_replacement_target_to_its_absent_state() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta();
        let working = root.join("data/apps/example/working");
        fs::remove_file(working.join("application.exe")).unwrap();
        let prepared = prepare_incremental(&delta, &root, &expected(), || false).unwrap();
        let error = prepared
            .apply(b"{\"version\":\"v1.4.9\"}", "{}", || {
                ProgramFiles::read(&working).is_ok_and(|manifest| manifest.version == "v1.4.9")
            })
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(!working.join("application.exe").exists());
        assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.8");
        assert_eq!(
            fs::read(root.join("data/apps/example/app.json")).unwrap(),
            b"{}"
        );
        assert!(!root.join(TASK_DIR).exists());
    }

    #[test]
    fn delta_deleting_a_program_file_that_became_a_directory_preserves_user_files() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta_deleting(&["Ä.dat"], &[]);
        let working = root.join("data/apps/example/working");
        let directory = working.join("Ä.dat");
        fs::remove_file(&directory).unwrap();
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("user.txt"), b"user data").unwrap();
        let prepared = prepare_incremental(&delta, &root, &expected(), || false).unwrap();
        prepared
            .apply(b"{\"version\":\"v1.4.9\"}", "{}", || false)
            .unwrap();
        assert_eq!(fs::read(directory.join("user.txt")).unwrap(), b"user data");
        let manifest = ProgramFiles::read(&working).unwrap();
        assert_eq!(manifest.version, "v1.4.9");
        assert!(!manifest.keys().contains(&key("Ä.dat")));
    }

    #[test]
    fn delta_refuses_a_replacement_directory_without_a_declared_conversion() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta();
        let working = root.join("data/apps/example/working");
        let directory = working.join("application.exe");
        fs::remove_file(&directory).unwrap();
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("user.txt"), b"user data").unwrap();
        let error = prepare_incremental(&delta, &root, &expected(), || false)
            .err()
            .unwrap();
        assert!(error.is::<NeedsFullPackage>());
        assert_eq!(fs::read(directory.join("user.txt")).unwrap(), b"user data");
        assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.8");
        assert!(!root.join(TASK_DIR).exists());
    }

    #[test]
    fn working_scope_matches_package_identity_even_when_unicode_case_changes_byte_length() {
        for (app_name, spelling) in [("example", "EXAMPLE"), ("Ä", "ä"), ("İ", "i\u{307}")] {
            let package = Package {
                app_name: app_name.into(),
                version: "v1.4.9".into(),
                resource_id: None,
                executable: "application.exe".into(),
                profiles: vec!["default".into()],
            };
            let name = format!("DATA/APPS/{spelling}/WORKING/folder/Resource.dat");
            assert!(package.allows(&name, false));
            assert_eq!(
                working_path(&name, &format!("{}/working/", package.base())),
                Some("folder/Resource.dat")
            );
        }
    }

    #[test]
    fn delta_case_variations_cannot_delete_user_files() {
        for path in [
            "DATA/APPS/EXAMPLE/WORKING/user.txt",
            "data/apps/example/Working/user.txt",
        ] {
            let fixture = Fixture::new();
            let (root, delta) = fixture.delta();
            let user = root.join("data/apps/example/working/user.txt");
            fs::write(&user, b"user data").unwrap();
            fixture.rewrite_zip(&delta, |entries| {
                let (_, bytes) = entries
                    .iter_mut()
                    .find(|(name, _)| name == "changes.json")
                    .unwrap();
                let mut changes: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                changes["deleted"] = serde_json::json!([path]);
                *bytes = serde_json::to_vec(&changes).unwrap();
            });
            let result = prepare_incremental(&delta, &root, &expected(), || false);
            assert!(
                result.is_err(),
                "accepted a user-file deletion through different root casing: {path}"
            );
            assert!(result
                .err()
                .unwrap()
                .to_string()
                .contains("untracked user file"));
            assert_eq!(fs::read(&user).unwrap(), b"user data");
            assert!(!root.join(TASK_DIR).exists());
        }
    }

    #[test]
    fn delta_case_variations_still_delete_owned_program_files() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta_deleting(&["Ä.dat"], &[]);
        fixture.rewrite_zip(&delta, |entries| {
            let (_, bytes) = entries
                .iter_mut()
                .find(|(name, _)| name == "changes.json")
                .unwrap();
            let mut changes: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            changes["deleted"] = serde_json::json!(["DATA/APPS/EXAMPLE/WORKING/Ä.dat"]);
            *bytes = serde_json::to_vec(&changes).unwrap();
        });
        prepare_incremental(&delta, &root, &expected(), || false)
            .unwrap()
            .apply(b"{\"version\":\"v1.4.9\"}", "{}", || false)
            .unwrap();
        let working = root.join("data/apps/example/working");
        assert!(!working.join("Ä.dat").exists());
        assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.9");
    }

    #[test]
    fn delta_with_a_missing_unchanged_resource_requires_full_body_without_publishing() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta();
        let working = root.join("data/apps/example/working");
        fs::remove_file(working.join("Ä.dat")).unwrap();
        let result = prepare_incremental(&delta, &root, &expected(), || false);
        assert!(
            result.is_err(),
            "accepted an incomplete projected program manifest"
        );
        assert!(result.err().unwrap().is::<NeedsFullPackage>());
        assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.8");
        assert_eq!(
            fs::read(working.join("application.exe")).unwrap(),
            b"fixture application"
        );
        assert!(!root.join(TASK_DIR).exists());
    }

    #[test]
    fn delta_with_unchanged_yaml_commits_the_body_version() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta();
        assert!(!root.join("pyappify-release.json").exists());
        let prepared = prepare_incremental(&delta, &root, &expected(), || false).unwrap();
        prepared
            .apply(b"{\"version\":\"v1.4.9\"}", "{}", || false)
            .unwrap();
        let journal: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(TASK_DIR).join("journal.json")).unwrap())
                .unwrap();
        assert_eq!(journal["rm_all_files"], true);
        assert_eq!(
            read_package(&root, "example").unwrap().unwrap().0.version,
            "v1.4.9"
        );
        assert!(!root
            .join("data/apps/example/.mirrorchyan-baseline.json")
            .exists());
        assert_eq!(
            fs::read(root.join("data/apps/example/working/application.exe")).unwrap(),
            b"updated fixture application"
        );
    }

    #[test]
    fn delta_with_a_missing_runtime_requires_a_full_body_and_keeps_the_old_version() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta();
        let working = root.join("data/apps/example/working");
        fs::remove_file(working.join("_internal/python312.dll")).unwrap();
        let error = prepare_incremental(&delta, &root, &expected(), || false)
            .err()
            .unwrap();
        assert!(error.is::<NeedsFullPackage>());
        assert!(!root.join(TASK_DIR).exists());
        assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.8");
        assert_eq!(
            fs::read(working.join("application.exe")).unwrap(),
            b"fixture application"
        );
    }

    #[test]
    fn delta_cannot_overwrite_a_file_owned_by_the_user() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta();
        let working = root.join("data/apps/example/working");
        let mut old = ProgramFiles::read(&working).unwrap();
        old.files.retain(|name| name != "application.exe");
        old.files.push("old-entry.exe".into());
        old.executable = "old-entry.exe".into();
        fs::write(working.join("old-entry.exe"), b"old entry").unwrap();
        fs::write(working.join("application.exe"), b"user data").unwrap();
        fs::write(
            working.join(MANIFEST_FILE),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        let error = prepare_incremental(&delta, &root, &expected(), || false)
            .err()
            .unwrap();
        assert!(error.to_string().contains("untracked user file"));
        assert_eq!(
            fs::read(working.join("application.exe")).unwrap(),
            b"user data"
        );
        assert_eq!(ProgramFiles::read(&working).unwrap().version, "v1.4.8");
        assert!(!transaction::replacement_pending(&root));
    }

    #[test]
    fn cancelled_delta_restores_the_version_with_the_body() {
        let fixture = Fixture::new();
        let (root, delta) = fixture.delta();
        let working = root.join("data/apps/example/working");
        let prepared = prepare_incremental(&delta, &root, &expected(), || false).unwrap();
        let error = prepared
            .apply(b"{\"version\":\"v1.4.9\"}", "{}", || {
                ProgramFiles::read(&working).is_ok_and(|manifest| manifest.version == "v1.4.9")
            })
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(
            read_package(&root, "example").unwrap().unwrap().0.version,
            "v1.4.8"
        );
        assert_eq!(
            fs::read(working.join("application.exe")).unwrap(),
            b"fixture application"
        );
        assert_eq!(
            fs::read(root.join("data/apps/example/app.json")).unwrap(),
            b"{}"
        );
    }
}
