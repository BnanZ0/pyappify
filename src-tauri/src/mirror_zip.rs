//! ZIP application is deliberately independent of the UI and network transport.
//! Full packages swap runtime/application trees; patches inspect only server paths.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
};

pub const PACKAGE_FILE: &str = "pyappify-release.json";
pub const ROOT_CONFIG_FILE: &str = "pyappify.yml";
pub const BASELINE_FILE: &str = ".mirrorchyan-baseline.json";
const TASK_DIR: &str = ".pyappify-update";
use crate::program_files::{self, ProgramFiles, MANIFEST_FILE};

pub fn replacement_pending(root: &Path) -> bool {
    let task = root.join(TASK_DIR);
    task.join("journal.json").is_file() && !task.join("committed").is_file()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Runtime {
    Pyinstaller { executable: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Package {
    pub format: u32,
    pub app_name: String,
    pub version: String,
    pub resource_id: String,
    pub launcher: String,
    pub runtime: Runtime,
    /// Profiles available in this frozen application.
    pub profiles: Vec<String>,
    /// Paths relative to working/, excluded from the release ZIP.
    #[serde(default)]
    pub preserve_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Baseline {
    pub package: Package,
    pub arch: String,
}

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

pub struct Expected<'a> {
    pub app_name: &'a str,
    pub resource_id: &'a str,
    pub version: &'a str,
    pub launcher: &'a str,
}

#[derive(Debug, thiserror::Error)]
#[error("A complete ZIP is required: {0}")]
pub struct NeedsFullPackage(pub String);

/// Reject Windows aliases as well as traversal, even when testing on another OS.
fn relative(value: &str) -> Result<PathBuf> {
    if value.is_empty() || value.contains('\\') {
        bail!("Invalid ZIP path: {value}");
    }
    let mut path = PathBuf::new();
    for component in value.split('/') {
        if component.is_empty()
            || matches!(component, "." | "..")
            || component.ends_with(['.', ' '])
            || component.chars().any(|c| c < ' ' || "<>:\"|?*".contains(c))
        {
            bail!("Invalid ZIP path: {value}");
        }
        let stem = component.split('.').next().unwrap().to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|n| {
                    matches!(
                        n,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            })
        {
            bail!("Reserved Windows ZIP path: {value}");
        }
        path.push(component);
    }
    Ok(path)
}

fn key(value: &str) -> String {
    value.to_lowercase()
}

fn within(value: &str, parent: &str) -> bool {
    let value = key(value);
    let parent = key(parent);
    value == parent || value.starts_with(&(parent + "/"))
}

fn is_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

/// Do not follow existing junctions or links during replacement or rollback.
fn safe_join(root: &Path, value: &str) -> Result<PathBuf> {
    let relative = relative(value)?;
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if is_reparse(&metadata) => {
                bail!("ZIP destination contains a link or junction: {value}")
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

impl Package {
    fn ignores_launcher(&self, value: &str) -> bool {
        value.eq_ignore_ascii_case(&self.launcher)
    }
    pub fn base(&self) -> String {
        format!("data/apps/{}", self.app_name)
    }

    pub fn executable(&self) -> &str {
        let Runtime::Pyinstaller { executable } = &self.runtime;
        executable
    }

    pub fn preserves_working_path(&self, value: &str) -> bool {
        self.preserve_paths.iter().any(|p| within(value, p)) && !within(value, "mid_lib/public")
    }

    pub fn skips_working_entry(&self, value: &str, directory: bool) -> bool {
        self.preserves_working_path(value) && !(directory && value.eq_ignore_ascii_case("mid_lib"))
    }

    pub fn protects_working_path(&self, value: &str) -> bool {
        self.preserves_working_path(value) || self.preserve_paths.iter().any(|p| within(p, value))
    }

    pub fn payload_present(&self, root: &Path) -> Result<bool> {
        let working = format!("{}/working", self.base());
        let config = safe_join(root, &format!("{working}/pyappify.yml"))?;
        let internal = safe_join(root, &format!("{working}/_internal"))?;
        let runtime_present = internal.is_dir()
            && fs::read_dir(&internal)?.any(|entry| {
                entry.is_ok_and(|entry| {
                    let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                    name.starts_with("python") && name.ends_with(".dll") && entry.path().is_file()
                })
            });
        Ok(config.is_file()
            && safe_join(root, &format!("{working}/{}", self.executable()))?.is_file()
            && runtime_present)
    }

    pub fn validate(&self, expected: &Expected<'_>) -> Result<()> {
        if self.format != 2
            || self.app_name != expected.app_name
            || self.resource_id != expected.resource_id
            || self.version != expected.version
            || !self.launcher.eq_ignore_ascii_case(expected.launcher)
            || self.profiles.is_empty()
        {
            bail!("ZIP release identity, version or launcher does not match this application");
        }
        if relative(&self.app_name)?.components().count() != 1
            || relative(&self.launcher)?.components().count() != 1
            || !self.launcher.to_ascii_lowercase().ends_with(".exe")
        {
            bail!("Invalid ZIP application or launcher name");
        }
        let executable = self.executable();
        if relative(executable)?.components().count() != 1
            || !executable.to_ascii_lowercase().ends_with(".exe")
            || self.preserve_paths.iter().any(|p| {
                within(executable, p)
                    || within("_internal", p)
                    || within(p, "_internal")
                    || within("pyappify.yml", p)
                    || within(p, "mid_lib/public")
            })
        {
            bail!("Invalid or preserved PyInstaller runtime path");
        }
        let mut preserved = BTreeSet::new();
        for path in &self.preserve_paths {
            relative(path)?;
            if !preserved.insert(key(path))
                || self
                    .preserve_paths
                    .iter()
                    .any(|other| path != other && within(path, other))
            {
                bail!("Overlapping ZIP preserve paths");
            }
        }
        Ok(())
    }

    pub fn allows(&self, value: &str, directory: bool) -> bool {
        if relative(value).is_err() {
            return false;
        }
        if value == PACKAGE_FILE || value.eq_ignore_ascii_case(&self.launcher) {
            return !directory;
        }
        if value == ROOT_CONFIG_FILE {
            return !directory;
        }
        let base = self.base();
        let working = format!("{base}/working");
        if directory && ["data", "data/apps", &base].contains(&value) {
            return true;
        }
        if !within(value, &working) {
            return false;
        }
        if !directory && key(value) == key(&working) {
            return false;
        }
        if value.split('/').any(|p| p.eq_ignore_ascii_case(".git")) {
            return false;
        }
        if key(value).ends_with("/.pip_update_needed.tmp") {
            return false;
        }
        let relative = value.get(working.len() + 1..).unwrap_or_default();
        !self.skips_working_entry(relative, directory)
            && (directory || !self.preserve_paths.iter().any(|p| within(p, relative)))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Action {
    Move {
        from: String,
        to: String,
    },
    Mkdir {
        path: String,
    },
    RemoveEmptyDir {
        path: String,
    },
    /// A fresh final installation directory; removed in full during rollback.
    FreshTree {
        path: String,
        backup: Option<String>,
    },
    Populate {
        from: String,
        to: String,
    },
}

#[derive(Serialize, Deserialize)]
struct Journal {
    #[serde(default)]
    package: Option<Package>,
    #[serde(default)]
    app_name: String,
    #[serde(default)]
    launcher: String,
    actions: Vec<Action>,
    rollback_config: Option<String>,
}

pub struct Prepared {
    pub package: Package,
    pub incremental: bool,
    pub validation_seconds: f64,
    pub extraction_seconds: f64,
    transaction: InstallTransaction,
}

/// Both distribution routes commit through the same durable file journal.
pub struct InstallTransaction {
    root: PathBuf,
    app_name: String,
    launcher: String,
    actions: Vec<Action>,
    displaced: BTreeSet<String>,
    created_dirs: BTreeSet<String>,
    applied: usize,
    migration: Option<Migration>,
}

struct Migration {
    backup: String,
    old: ProgramFiles,
    new: ProgramFiles,
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

fn check_cancelled(cancelled: &impl Fn() -> bool) -> Result<()> {
    if cancelled() {
        bail!("Operation cancelled by user");
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(reader: impl Read, limit: u64) -> Result<T> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("ZIP metadata is too large");
    }
    Ok(serde_json::from_slice(&bytes).context("Invalid ZIP metadata")?)
}

pub fn read_package(root: &Path) -> Result<Option<Package>> {
    let path = safe_join(root, PACKAGE_FILE)?;
    if !path.exists() {
        return Ok(None);
    }
    let package: Package = read_json(fs::File::open(path)?, 64 * 1024)?;
    package.validate(&Expected {
        app_name: &package.app_name,
        resource_id: &package.resource_id,
        version: &package.version,
        launcher: &package.launcher,
    })?;
    Ok(Some(package))
}

pub fn baseline(root: &Path, app_name: &str) -> Result<Option<Baseline>> {
    let path = safe_join(root, &format!("data/apps/{app_name}/{BASELINE_FILE}"))?;
    if !path.exists() {
        return Ok(None);
    }
    // Corrupt or outdated baseline metadata requires a full package.
    Ok(read_json(fs::File::open(path)?, 64 * 1024).ok())
}

pub fn prepare(
    archive_path: &Path,
    root: &Path,
    expected: &Expected<'_>,
    allow_incremental: bool,
    cancelled: impl Fn() -> bool,
) -> Result<Prepared> {
    let validation_started = std::time::Instant::now();
    check_cancelled(&cancelled)?;
    let mut archive = zip::ZipArchive::new(fs::File::open(archive_path)?)
        .context("The update is not a valid ZIP package")?;
    let package: Package = read_json(
        archive
            .by_name(PACKAGE_FILE)
            .context("Missing pyappify-release.json in ZIP")?,
        64 * 1024,
    )?;
    package.validate(expected)?;
    let changes = match archive.by_name("changes.json") {
        Ok(file) => Some(read_json::<Changes>(file, 16 * 1024 * 1024)?),
        Err(zip::result::ZipError::FileNotFound) => None,
        Err(error) => return Err(error.into()),
    };
    if changes.is_some() && !allow_incremental {
        return Err(
            NeedsFullPackage("the installed directory has no trusted ZIP baseline".into()).into(),
        );
    }
    if changes.is_some()
        && read_package(root)?.is_some_and(|old| {
            old.runtime != package.runtime || old.preserve_paths != package.preserve_paths
        })
    {
        return Err(NeedsFullPackage(
            "the runtime or user-data preservation layout changed".into(),
        )
        .into());
    }
    let mut entries = BTreeMap::new();
    let mut names = BTreeSet::new();
    let mut spellings = BTreeMap::new();
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
            continue;
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
                if !directory_to_file
                    && (destination.exists() == added
                        || (destination.exists() && !destination.is_file()))
                {
                    return Err(NeedsFullPackage(format!("baseline file differs: {name}")).into());
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
                    || [
                        "data",
                        "data/apps",
                        &base,
                        &format!("{base}/working"),
                        PACKAGE_FILE,
                        ROOT_CONFIG_FILE,
                        &package.launcher,
                    ]
                    .iter()
                    .any(|root| root.eq_ignore_ascii_case(name))
                    || (within(name, &format!("{base}/working"))
                        && package.protects_working_path(
                            name.get(base.len() + "/working/".len()..)
                                .unwrap_or_default(),
                        ))
                    || within(&format!("{base}/working/{}", package.executable()), name)
                    || within(&format!("{base}/working/_internal"), name)
                    || (writes.contains(&key(name)) && !(directory && changes.added.contains(name)))
                    || !deletions.insert(key(name))
                {
                    bail!("Invalid changes.json deletion: {name}");
                }
                safe_join(root, name)?;
                if !directory {
                    if let Some(relative) = name.strip_prefix(&working_prefix) {
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
            format!("{base}/working/pyappify.yml"),
            ROOT_CONFIG_FILE.into(),
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
    if task.exists() {
        bail!("An unfinished ZIP transaction must be recovered before updating");
    }
    fs::create_dir_all(task.join("staged"))?;
    let validation_seconds = validation_started.elapsed().as_secs_f64();
    let extraction_started = std::time::Instant::now();
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
            }
            output.flush()?;
        }
        Ok(())
    })();
    if let Err(error) = extraction {
        let _ = fs::remove_dir_all(&task);
        return Err(error);
    }
    if changes.is_none() && !package.payload_present(&task.join("staged"))? {
        let _ = fs::remove_dir_all(&task);
        bail!("Incomplete PyInstaller ZIP: executable, configuration or Python DLL is missing");
    }
    let staged_body = task.join("staged").join(&base).join("working");
    let new_program = if staged_body.join(MANIFEST_FILE).is_file() {
        ProgramFiles::read(&staged_body)?
    } else if let Some(old) = &old_program {
        old.clone()
    } else {
        bail!("Missing pyappify-files.json in the application body");
    };
    let mut expected_files = if let Some(old) = &old_program {
        old.keys()
    } else {
        BTreeSet::new()
    };
    if let Some(changes) = &changes {
        for name in changes.deleted.iter().chain(&changes.deleted_dir) {
            if let Some(path) = name.strip_prefix(&working_prefix) {
                expected_files.retain(|name| !within(name, path));
            }
        }
    }
    for (_, name, directory) in entries.values() {
        if !directory {
            if let Some(path) = name.strip_prefix(&working_prefix) {
                expected_files.insert(key(path));
            }
        }
    }
    if new_program.keys() != expected_files {
        bail!("The application program manifest does not match the ZIP files and changes");
    }
    let mut prepared = Prepared {
        transaction: InstallTransaction::new(root, &package.app_name, &package.launcher)?,
        package,
        incremental: changes.is_some(),
        validation_seconds,
        extraction_seconds: extraction_started.elapsed().as_secs_f64(),
    };
    if prepared.incremental {
        for name in deleted {
            // Removing a program directory must leave untracked user files inside it.
            let directory_to_file = changes.as_ref().is_some_and(|c| c.added.contains(&name));
            if safe_join(root, &name)?.is_dir() && !directory_to_file {
                if let Some(path) = name.strip_prefix(&working_prefix) {
                    for file in &old_program.as_ref().unwrap().files {
                        if within(file, path) {
                            prepared.backup(&format!("{working_prefix}{file}"))?;
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
        let old_program = program_files::installed(root, &prepared.package.app_name)?;
        let backup = prepared.fresh_tree(&format!("{base}/working"))?;
        if let (Some(backup), Some(old)) = (backup, old_program) {
            prepared.migration = Some(Migration {
                backup,
                old,
                new: new_program,
            });
        }
        prepared.actions.push(Action::Populate {
            from: format!("{TASK_DIR}/staged/{base}/working"),
            to: format!("{base}/working"),
        });
        // Old Git program files are retired only by the committed transaction.
        prepared.backup(&format!("{base}/python"))?;
        prepared.backup(&format!("{base}/repo"))?;
        prepared.replace(PACKAGE_FILE)?;
        prepared.replace(ROOT_CONFIG_FILE)?;
    }
    Ok(prepared)
}

impl InstallTransaction {
    fn new(root: &Path, app_name: &str, launcher: &str) -> Result<Self> {
        if relative(app_name)?.components().count() != 1
            || relative(launcher)?.components().count() != 1
        {
            bail!("Invalid installation identity");
        }
        Ok(Self {
            root: root.into(),
            app_name: app_name.into(),
            launcher: launcher.into(),
            actions: Vec::new(),
            displaced: BTreeSet::new(),
            created_dirs: BTreeSet::new(),
            applied: 0,
            migration: None,
        })
    }

    fn base(&self) -> String {
        format!("data/apps/{}", self.app_name)
    }

    /// Stop the app before calling this. Installers use these final paths directly.
    pub fn begin_git(
        root: &Path,
        app_name: &str,
        launcher: &str,
        rollback_config: &str,
        cancelled: impl Fn() -> bool,
    ) -> Result<Self> {
        if safe_join(root, TASK_DIR)?.exists() {
            bail!("Recover the previous installation before installing");
        }
        let old_program = program_files::installed(root, app_name)?;
        let mut transaction = Self::new(root, app_name, launcher)?;
        fs::create_dir_all(root.join(TASK_DIR).join("staged"))?;
        let base = transaction.base();
        let backup = transaction.fresh_tree(&format!("{base}/working"))?;
        if let (Some(backup), Some(old)) = (backup, old_program) {
            transaction.migration = Some(Migration {
                backup,
                old,
                new: ProgramFiles::new(Vec::new())?,
            });
        }
        transaction.fresh_tree(&format!("{base}/python"))?;
        transaction.fresh_tree(&format!("{base}/repo"))?;
        transaction.backup(PACKAGE_FILE)?;
        transaction.backup(ROOT_CONFIG_FILE)?;
        transaction.backup(&format!("{base}/{BASELINE_FILE}"))?;
        let journal = transaction.journal(rollback_config, None);
        validate_journal(&journal)?;
        durable_write(
            &root.join(TASK_DIR).join("journal.json"),
            &serde_json::to_vec(&journal)?,
        )?;
        for action in &transaction.actions {
            check_cancelled(&cancelled)?;
            apply_action(root, action)?;
        }
        transaction.applied = transaction.actions.len();
        Ok(transaction)
    }

    fn fresh_tree(&mut self, value: &str) -> Result<Option<String>> {
        let backup = self.backup(value)?;
        self.parents(value)?;
        self.actions.push(Action::FreshTree {
            path: value.into(),
            backup: backup.clone(),
        });
        Ok(backup)
    }

    pub fn prepare_git_commit(&mut self, new_program: ProgramFiles) -> Result<()> {
        let working = self.root.join(self.base()).join("working");
        if !self
            .root
            .join(self.base())
            .join("python/python.exe")
            .is_file()
            || !working.join(ROOT_CONFIG_FILE).is_file()
        {
            bail!("Incomplete Git installation");
        }
        if let Some(migration) = &mut self.migration {
            migration.new = new_program;
        }
        fs::copy(
            working.join(ROOT_CONFIG_FILE),
            self.root
                .join(TASK_DIR)
                .join("staged")
                .join(ROOT_CONFIG_FILE),
        )?;
        self.replace(ROOT_CONFIG_FILE)?;
        Ok(())
    }

    fn journal(&self, rollback_config: &str, package: Option<Package>) -> Journal {
        Journal {
            package,
            app_name: self.app_name.clone(),
            launcher: self.launcher.clone(),
            actions: self.actions.clone(),
            rollback_config: Some(rollback_config.into()),
        }
    }
    /// Existing paths whose payload/state will move. The RM collector decides
    /// whether to enumerate a changed subtree or only full-swap entry points.
    pub fn replacement_paths(&self) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for action in &self.actions {
            if let Action::Move { from, to } = action {
                if !within(from, TASK_DIR) && within(to, TASK_DIR) {
                    paths.push(safe_join(&self.root, from)?);
                }
            }
        }
        for name in [
            format!("{}/app.json", self.base()),
            format!("{}/{BASELINE_FILE}", self.base()),
        ] {
            paths.push(safe_join(&self.root, &name)?);
        }
        Ok(paths)
    }
    fn already_displaced(&self, value: &str) -> bool {
        let mut path = Some(Path::new(value));
        while let Some(current) = path {
            if self
                .displaced
                .contains(&key(&current.to_string_lossy().replace('\\', "/")))
            {
                return true;
            }
            path = current.parent();
        }
        false
    }

    fn mkdir(&mut self, value: &str) -> Result<()> {
        self.parents(value)?;
        if self.created_dirs.contains(&key(value)) {
            return Ok(());
        }
        let path = safe_join(&self.root, value)?;
        if !path.is_dir() || self.already_displaced(value) {
            self.actions.push(Action::Mkdir { path: value.into() });
            self.created_dirs.insert(key(value));
        }
        Ok(())
    }

    fn parents(&mut self, value: &str) -> Result<()> {
        if within(value, TASK_DIR) {
            return Ok(());
        }
        let parent = value.rsplit_once('/').map(|(parent, _)| parent);
        if let Some(parent) = parent {
            self.mkdir(parent)?;
        }
        Ok(())
    }

    fn backup(&mut self, value: &str) -> Result<Option<String>> {
        if !self.already_displaced(value) && safe_join(&self.root, value)?.exists() {
            let backup = format!("{TASK_DIR}/backup/{}", self.actions.len());
            self.actions.push(Action::Move {
                from: value.into(),
                to: backup.clone(),
            });
            self.displaced.insert(key(value));
            Ok(Some(backup))
        } else {
            Ok(None)
        }
    }

    fn replace(&mut self, value: &str) -> Result<()> {
        self.backup(value)?;
        self.parents(value)?;
        self.actions.push(Action::Move {
            from: format!("{TASK_DIR}/staged/{value}"),
            to: value.into(),
        });
        Ok(())
    }
}

impl Prepared {
    pub fn replacement_paths(&self) -> Result<Vec<PathBuf>> {
        let mut paths = self.transaction.replacement_paths()?;
        if !self.incremental {
            paths.push(safe_join(
                &self.root,
                &format!("{}/working/_internal", self.package.base()),
            )?);
        }
        Ok(paths)
    }

    pub fn working_config(&self) -> PathBuf {
        let relative = format!("{}/working/pyappify.yml", self.package.base());
        let staged = self.root.join(TASK_DIR).join("staged").join(&relative);
        if staged.exists() {
            staged
        } else {
            self.root.join(relative)
        }
    }

    pub fn staged_root_config(&self) -> Option<PathBuf> {
        let path = self
            .root
            .join(TASK_DIR)
            .join("staged")
            .join(ROOT_CONFIG_FILE);
        path.is_file().then_some(path)
    }

    /// Config and baseline are part of the same rollback journal as the payload.
    pub fn apply(
        self,
        config: &[u8],
        rollback_config: &str,
        cancelled: impl Fn() -> bool,
    ) -> Result<()> {
        let baseline = Baseline {
            package: self.package.clone(),
            arch: std::env::consts::ARCH.into(),
        };
        self.transaction
            .apply(config, rollback_config, Some(baseline), cancelled)
    }
}

impl InstallTransaction {
    pub fn apply(
        mut self,
        config: &[u8],
        rollback_config: &str,
        baseline: Option<Baseline>,
        cancelled: impl Fn() -> bool,
    ) -> Result<()> {
        let base = self.base();
        let mut state_files = vec![(format!("{base}/app.json"), config.to_vec())];
        if let Some(record) = &baseline {
            state_files.push((
                format!("{base}/{BASELINE_FILE}"),
                serde_json::to_vec(record)?,
            ));
        }
        for (name, bytes) in state_files {
            let path = self.root.join(TASK_DIR).join("staged").join(&name);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, bytes)?;
            self.replace(&name)?;
        }
        let journal = self.journal(rollback_config, baseline.map(|record| record.package));
        let task = self.root.join(TASK_DIR);
        validate_journal(&journal)?;
        durable_write(&task.join("journal.json"), &serde_json::to_vec(&journal)?)?;
        let result = (|| -> Result<()> {
            for action in journal.actions.iter().skip(self.applied) {
                check_cancelled(&cancelled)?;
                apply_action(&self.root, action)?;
            }
            if let Some(migration) = &self.migration {
                program_files::migrate_users(
                    &safe_join(&self.root, &migration.backup)?,
                    &self.root.join(self.base()).join("working"),
                    &migration.old,
                    &migration.new,
                    &cancelled,
                )?;
            }
            // Once committed, cancellation must not reverse a successful update.
            check_cancelled(&cancelled)?;
            durable_write(&task.join("committed"), b"1")?;
            Ok(())
        })();
        if let Err(error) = result {
            rollback(&self.root, &journal).context(format!(
                "File replacement failed ({error:#}); rollback also failed"
            ))?;
            remove_retired_tree(&task)
                .context("Files were restored, but staging cleanup is incomplete")?;
            return Err(error);
        }
        // Cleanup failure cannot turn a committed update into a failed one.
        let _ = retire(&self.root);
        Ok(())
    }
}

fn apply_action(root: &Path, action: &Action) -> Result<()> {
    match action {
        Action::Move { from, to } => {
            let source = safe_join(root, from)?;
            let destination = safe_join(root, to)?;
            fs::create_dir_all(destination.parent().unwrap())?;
            if destination.exists() {
                bail!("Installation destination unexpectedly exists: {to}");
            }
            fs::rename(source, destination).with_context(|| {
                format!("Unable to replace {from}; close applications using this file and retry")
            })?;
        }
        Action::Mkdir { path } | Action::FreshTree { path, .. } => {
            fs::create_dir_all(safe_join(root, path)?)?;
        }
        Action::RemoveEmptyDir { path } => match fs::remove_dir(safe_join(root, path)?) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => return Err(error.into()),
        },
        Action::Populate { from, to } => {
            for entry in fs::read_dir(safe_join(root, from)?)? {
                let entry = entry?;
                let name = entry.file_name();
                fs::rename(entry.path(), safe_join(root, to)?.join(name))?;
            }
        }
    }
    Ok(())
}

fn durable_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("{:x}.tmp", rand::random::<u64>()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let wide = |path: &Path| {
            path.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        };
        if unsafe {
            MoveFileExW(
                wide(&temporary).as_ptr(),
                wide(path).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            let error = std::io::Error::last_os_error();
            let _ = fs::remove_file(temporary);
            return Err(error.into());
        }
    }
    #[cfg(not(windows))]
    fs::rename(temporary, path)?;
    Ok(())
}

fn validate_journal(journal: &Journal) -> Result<()> {
    let (app_name, launcher) = if let Some(package) = &journal.package {
        package.validate(&Expected {
            app_name: &package.app_name,
            resource_id: &package.resource_id,
            version: &package.version,
            launcher: &package.launcher,
        })?;
        (&package.app_name, &package.launcher)
    } else {
        (&journal.app_name, &journal.launcher)
    };
    if relative(app_name)?.components().count() != 1
        || relative(launcher)?.components().count() != 1
    {
        bail!("Invalid installation recovery identity");
    }
    let base = format!("data/apps/{app_name}");
    for action in &journal.actions {
        let paths = match action {
            Action::Move { from, to } => vec![from, to],
            Action::Mkdir { path } | Action::RemoveEmptyDir { path } => vec![path],
            Action::FreshTree { path, backup } => {
                if ![
                    format!("{base}/working"),
                    format!("{base}/python"),
                    format!("{base}/repo"),
                ]
                .contains(path)
                    || backup.as_ref().is_some_and(|p| !within(p, TASK_DIR))
                {
                    bail!("Invalid installation recovery tree");
                }
                let mut paths = vec![path];
                paths.extend(backup.iter());
                paths
            }
            Action::Populate { from, to } => {
                if !within(from, &format!("{TASK_DIR}/staged")) || to != &format!("{base}/working")
                {
                    bail!("Invalid installation recovery payload");
                }
                vec![from, to]
            }
        };
        for path in paths {
            relative(path)?;
            if path.eq_ignore_ascii_case(launcher) {
                bail!("The launcher cannot be moved by a ZIP recovery journal");
            }
            if !within(path, TASK_DIR)
                && !journal
                    .package
                    .as_ref()
                    .is_some_and(|p| p.allows(path, true) || p.allows(path, false))
                && ![
                    "data".into(),
                    "data/apps".into(),
                    base.clone(),
                    format!("{base}/working"),
                    format!("{base}/python"),
                    format!("{base}/repo"),
                    format!("{base}/app.json"),
                    format!("{base}/{BASELINE_FILE}"),
                    PACKAGE_FILE.into(),
                    ROOT_CONFIG_FILE.into(),
                ]
                .contains(path)
            {
                bail!("Invalid ZIP recovery journal path");
            }
        }
    }
    Ok(())
}

fn rollback(root: &Path, journal: &Journal) -> Result<()> {
    validate_journal(journal)?;
    for action in journal.actions.iter().rev() {
        match action {
            Action::Move { from, to } => {
                let source = safe_join(root, from)?;
                let destination = safe_join(root, to)?;
                // Each move has a unique empty destination. Presence tells us
                // whether it happened, without flushing a journal for every file.
                if !source.exists() && destination.exists() {
                    fs::create_dir_all(source.parent().unwrap())?;
                    fs::rename(destination, source)
                        .with_context(|| format!("Unable to restore {from}"))?;
                }
            }
            Action::Mkdir { path } => {
                let _ = fs::remove_dir(safe_join(root, path)?);
            }
            Action::RemoveEmptyDir { path } => {
                fs::create_dir_all(safe_join(root, path)?)?;
            }
            Action::FreshTree { path, backup } => {
                // A failed backup move must never cause removal of the old tree.
                if backup
                    .as_ref()
                    .map(|p| safe_join(root, p).map(|p| p.exists()))
                    .transpose()?
                    .unwrap_or(true)
                {
                    let path = safe_join(root, path)?;
                    if path.exists() {
                        remove_retired_tree(&path)?;
                    }
                }
            }
            Action::Populate { .. } => {} // The following FreshTree rollback owns these files.
        }
    }
    if let Some(config) = &journal.rollback_config {
        let app_name = journal
            .package
            .as_ref()
            .map(|p| &p.app_name)
            .unwrap_or(&journal.app_name);
        let path = safe_join(root, &format!("data/apps/{app_name}/app.json"))?;
        fs::create_dir_all(path.parent().unwrap())?;
        durable_write(&path, config.as_bytes())?;
    }
    Ok(())
}

fn retire(root: &Path) -> Result<()> {
    let task = safe_join(root, TASK_DIR)?;
    let retired = root.join(format!("{TASK_DIR}-completed-{:x}", rand::random::<u64>()));
    fs::rename(task, retired)?;
    Ok(())
}

/// Run before reading app.json, so an interrupted apply cannot publish a new
/// version with old files. Downloads/extraction have no journal and are disposable.
pub fn recover(root: &Path) -> Result<bool> {
    let task = safe_join(root, TASK_DIR)?;
    if !task.exists() {
        return Ok(false);
    }
    if task.join("committed").exists() {
        if let Err(error) = retire(root) {
            tracing::warn!("Installed payload is ready; backup cleanup is incomplete: {error:#}");
        }
        return Ok(false);
    }
    let journal_path = task.join("journal.json");
    if journal_path.exists() {
        let journal: Journal = read_json(fs::File::open(journal_path)?, 32 * 1024 * 1024)?;
        rollback(root, &journal)?;
        remove_retired_tree(&task)?;
        return Ok(true);
    }
    remove_retired_tree(&task)?;
    Ok(false)
}

/// Remove committed backups outside apply timing; retry occupied files on boot.
pub fn cleanup(root: &Path) -> Result<()> {
    if root.join(TASK_DIR).join("committed").is_file() {
        retire(root).context("Installation succeeded, but backup cleanup is incomplete")?;
    }
    let mut errors = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{TASK_DIR}-completed-"))
        {
            if !is_reparse(&fs::symlink_metadata(entry.path())?)
                && entry.path().join("committed").is_file()
            {
                if let Err(error) = remove_retired_tree(&entry.path()) {
                    errors.push(format!("{}: {error:#}", entry.path().display()));
                }
            }
        }
    }
    if !errors.is_empty() {
        bail!(
            "Installation succeeded, but backup cleanup is incomplete: {}",
            errors.join("; ")
        );
    }
    Ok(())
}

fn remove_retired_tree(path: &Path) -> Result<()> {
    for attempt in 0..5 {
        match remove_disposable_tree_once(path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let transient = error
                    .root_cause()
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| {
                        error.kind() == std::io::ErrorKind::PermissionDenied
                            || matches!(error.raw_os_error(), Some(32 | 33 | 145 | 1224))
                    });
                if !transient || attempt == 4 {
                    return Err(error);
                }
                // Brief retries apply only to disposable files, never to live app locks.
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }
    unreachable!()
}

fn remove_disposable_tree_once(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            #[cfg(not(windows))]
            {
                return Err(error.into());
            }
            #[cfg(windows)]
            {
                if error.kind() != std::io::ErrorKind::PermissionDenied {
                    return Err(error.into());
                }
            }
        }
    }
    #[cfg(windows)]
    {
        // Disposable staging or a successfully rolled back/committed backup only.
        // Live caches retain their attributes; occupied files are never force-unlocked.
        for entry in walkdir::WalkDir::new(path) {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_reparse(&metadata) {
                bail!("Retired ZIP backup contains a junction or link");
            }
            if metadata.is_file() && metadata.permissions().readonly() {
                let mut permissions = metadata.permissions();
                permissions.set_readonly(false);
                fs::set_permissions(entry.path(), permissions)?;
            }
        }
        fs::remove_dir_all(path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use zip::write::SimpleFileOptions;

    struct Fixture {
        directory: PathBuf,
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("mirror-zip-tests")
                .join(format!("{:x}", rand::random::<u64>()));
            let root = directory.join("install");
            fs::create_dir_all(root.join("data/apps/sample")).unwrap();
            fs::write(
                root.join("data/apps/sample/app.json"),
                Self::config("v0.0.0"),
            )
            .unwrap();
            fs::write(root.join("sample.exe"), b"current launcher").unwrap();
            Self { directory, root }
        }
        fn package(version: &str) -> Package {
            Package {
                format: 2,
                app_name: "sample".into(),
                resource_id: "sample".into(),
                version: version.into(),
                launcher: "sample.exe".into(),
                runtime: Runtime::Pyinstaller {
                    executable: "sample.exe".into(),
                },
                profiles: vec!["China".into()],
                preserve_paths: ["configs", "custom_chars", "cache", "mid_lib", "user/nested"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            }
        }
        fn config(version: &str) -> Vec<u8> {
            serde_json::to_vec(&serde_json::json!({"current_version":version, "auto_start":true,"update_method":"MANUAL_UPDATE","current_profile":"China"})).unwrap()
        }
        fn zip(
            &self,
            name: &str,
            version: &str,
            changes: Option<serde_json::Value>,
            files: &[(&str, &[u8])],
        ) -> PathBuf {
            self.zip_package(name, &Self::package(version), changes, files)
        }
        fn zip_package(
            &self,
            name: &str,
            package: &Package,
            mut changes: Option<serde_json::Value>,
            files: &[(&str, &[u8])],
        ) -> PathBuf {
            let path = self.directory.join(name);
            let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(PACKAGE_FILE, options).unwrap();
            zip.write_all(&serde_json::to_vec(package).unwrap())
                .unwrap();
            let prefix = format!("{}/working/", package.base());
            let manifest_path = format!("{prefix}{MANIFEST_FILE}");
            let mut program = if changes.is_some() {
                ProgramFiles::read(&self.root.join(&package.base()).join("working"))
                    .unwrap()
                    .files
            } else {
                Vec::new()
            };
            if let Some(changes) = &mut changes {
                for deleted in ["deleted", "deleted_dir"]
                    .into_iter()
                    .flat_map(|field| changes[field].as_array().into_iter().flatten())
                {
                    if let Some(path) = deleted.as_str().and_then(|path| path.strip_prefix(&prefix))
                    {
                        program.retain(|name| !within(name, path));
                    }
                }
                let field = if self.root.join(&manifest_path).is_file() {
                    "modified"
                } else {
                    "added"
                };
                if !changes[field].is_array() {
                    changes[field] = serde_json::json!([]);
                }
                changes[field]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!(manifest_path));
            }
            for (name, _) in files {
                if let Some(path) = name.strip_prefix(&prefix) {
                    if !program.iter().any(|old| key(old) == key(path)) {
                        program.push(path.into());
                    }
                }
            }
            if package.version == "v1.0.0"
                && files.iter().any(|(name, _)| name.ends_with("/sample.exe"))
            {
                let name = format!("{prefix}mid_lib/public/old.mid");
                zip.start_file(&name, options).unwrap();
                zip.write_all(b"old built-in").unwrap();
                program.push("mid_lib/public/old.mid".into());
                zip.start_file(format!("{prefix}mid_lib/public/obsolete.mid"), options)
                    .unwrap();
                zip.write_all(b"old bundled MIDI").unwrap();
                program.push("mid_lib/public/obsolete.mid".into());
            }
            if !program.iter().any(|p| p == MANIFEST_FILE) {
                program.push(MANIFEST_FILE.into());
            }
            // Malformed-path tests deliberately write invalid manifest entries.
            let program = ProgramFiles::new(program.clone()).unwrap_or(ProgramFiles {
                format: 1,
                files: program,
            });
            zip.start_file(&manifest_path, options).unwrap();
            zip.write_all(&serde_json::to_vec(&program).unwrap())
                .unwrap();
            if let Some(changes) = changes {
                zip.start_file("changes.json", options).unwrap();
                zip.write_all(&serde_json::to_vec(&changes).unwrap())
                    .unwrap();
            }
            for (name, bytes) in files {
                zip.start_file(*name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
            path
        }
        fn full(&self, version: &str) -> PathBuf {
            self.zip(
                "full.zip",
                version,
                None,
                &[
                    ("sample.exe", b"new launcher"),
                    (ROOT_CONFIG_FILE, b"name: sample"),
                    ("data/apps/sample/working/sample.exe", b"frozen application"),
                    (
                        "data/apps/sample/working/_internal/python312.dll",
                        b"embedded Python",
                    ),
                    (
                        "data/apps/sample/working/_internal/library/native.pyd",
                        b"library unchanged",
                    ),
                    ("data/apps/sample/working/pyappify.yml", b"name: sample"),
                    ("data/apps/sample/working/README.md", b"old main"),
                    ("data/apps/sample/working/removed/old.py", b"old file"),
                    (
                        "data/apps/sample/working/mid_lib/public/new.mid",
                        b"bundled MIDI",
                    ),
                ],
            )
        }
        fn prepare(&self, archive: &Path, version: &str, patch: bool) -> Result<Prepared> {
            prepare(
                archive,
                &self.root,
                &Expected {
                    app_name: "sample",
                    resource_id: "sample",
                    version,
                    launcher: "sample.exe",
                },
                patch,
                || false,
            )
        }
        fn apply(&self, prepared: Prepared, version: &str) -> Result<()> {
            let old = fs::read_to_string(self.root.join("data/apps/sample/app.json"))?;
            prepared.apply(&Self::config(version), &old, || false)
        }
        fn install(&self) {
            self.apply(
                self.prepare(&self.full("v1.0.0"), "v1.0.0", false).unwrap(),
                "v1.0.0",
            )
            .unwrap();
            cleanup(&self.root).unwrap();
        }
        fn read(&self, path: &str) -> Vec<u8> {
            fs::read(self.root.join(path)).unwrap()
        }
    }

    #[test]
    fn git_to_mirror_prepares_privately_then_retires_git_files_at_commit() {
        let fixture = Fixture::new();
        install_old_git(&fixture);
        let before = payload_snapshot(&fixture);
        let prepared = fixture
            .prepare(&fixture.full("v2.0.0"), "v2.0.0", false)
            .unwrap();
        assert_eq!(payload_snapshot(&fixture), before);
        fixture.apply(prepared, "v2.0.0").unwrap();
        assert!(!fixture.root.join("data/apps/sample/python").exists());
        assert!(!fixture.root.join("data/apps/sample/repo").exists());
        assert!(!fixture
            .root
            .join("data/apps/sample/working/main.py")
            .exists());
        assert!(Fixture::package("v2.0.0")
            .payload_present(&fixture.root)
            .unwrap());
        assert_personal_data(&fixture);
        assert_eq!(
            fixture.read("data/apps/sample/working/mid_lib/public/new.mid"),
            b"bundled MIDI"
        );
        assert!(!fixture
            .root
            .join("data/apps/sample/working/mid_lib/public/old.mid")
            .exists());
        assert_eq!(fixture.read("sample.exe"), b"current launcher");
    }

    #[test]
    fn mirror_to_git_installs_at_final_paths_then_commits_record() {
        let fixture = Fixture::new();
        fixture.install();
        write_personal_data(&fixture);
        let before = payload_snapshot(&fixture);
        let prepared = prepare_new_git(&fixture);
        assert_ne!(payload_snapshot(&fixture), before);
        assert!(fixture.root.join(TASK_DIR).join("journal.json").is_file());
        let old_config = fixture.read("data/apps/sample/app.json");
        prepared
            .apply(
                &Fixture::config("v2.0.0"),
                std::str::from_utf8(&old_config).unwrap(),
                None,
                || false,
            )
            .unwrap();
        assert!(!fixture.root.join(PACKAGE_FILE).exists());
        assert!(baseline(&fixture.root, "sample").unwrap().is_none());
        assert!(!fixture
            .root
            .join("data/apps/sample/working/_internal")
            .exists());
        assert!(!fixture
            .root
            .join("data/apps/sample/working/sample.exe")
            .exists());
        assert_eq!(
            fixture.read("data/apps/sample/python/python.exe"),
            b"prepared Python"
        );
        assert_eq!(
            fixture.read("data/apps/sample/repo/main.py"),
            b"new Git program"
        );
        assert_eq!(
            fixture.read("data/apps/sample/working/main.py"),
            b"new Git program"
        );
        assert_eq!(
            fixture.read("data/apps/sample/app.json"),
            Fixture::config("v2.0.0")
        );
        assert_personal_data(&fixture);
        assert_eq!(
            fixture.read("data/apps/sample/working/mid_lib/public/new.mid"),
            b"new Git MIDI"
        );
        assert_eq!(fixture.read("sample.exe"), b"current launcher");
    }

    #[test]
    fn source_switch_cancellation_restores_files_and_record_at_early_middle_and_final_moves() {
        for to_mirror in [true, false] {
            for checkpoint in [0, 1, 4, 8, usize::MAX] {
                let fixture = Fixture::new();
                if to_mirror {
                    install_old_git(&fixture);
                } else {
                    fixture.install();
                    write_personal_data(&fixture);
                }
                let before = payload_snapshot(&fixture);
                let old_config = fixture.read("data/apps/sample/app.json");
                let calls = Cell::new(0);
                let result = if to_mirror {
                    let prepared = fixture
                        .prepare(&fixture.full("v2.0.0"), "v2.0.0", false)
                        .unwrap();
                    let checkpoint = checkpoint.min(prepared.actions.len() + 3);
                    prepared.apply(
                        &Fixture::config("v2.0.0"),
                        std::str::from_utf8(&old_config).unwrap(),
                        || {
                            calls.set(calls.get() + 1);
                            calls.get() > checkpoint
                        },
                    )
                } else {
                    let prepared = prepare_new_git(&fixture);
                    let checkpoint = checkpoint.min(prepared.actions.len() + 2);
                    prepared.apply(
                        &Fixture::config("v2.0.0"),
                        std::str::from_utf8(&old_config).unwrap(),
                        None,
                        || {
                            calls.set(calls.get() + 1);
                            calls.get() > checkpoint
                        },
                    )
                };
                assert!(
                    result.is_err(),
                    "to_mirror={to_mirror}, checkpoint={checkpoint}"
                );
                recover(&fixture.root).unwrap();
                assert_eq!(payload_snapshot(&fixture), before);
                assert_personal_data(&fixture);
                assert!(!replacement_pending(&fixture.root));
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn source_switch_late_record_lock_restores_old_program_and_record_both_directions() {
        use std::os::windows::fs::OpenOptionsExt;
        for to_mirror in [true, false] {
            let fixture = Fixture::new();
            if to_mirror {
                install_old_git(&fixture);
            } else {
                fixture.install();
                write_personal_data(&fixture);
            }
            let before = payload_snapshot(&fixture);
            let old_config = fixture.read("data/apps/sample/app.json");
            let lock = fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(fixture.root.join("data/apps/sample/app.json"))
                .unwrap();
            let result = if to_mirror {
                fixture
                    .prepare(&fixture.full("v2.0.0"), "v2.0.0", false)
                    .unwrap()
                    .apply(
                        &Fixture::config("v2.0.0"),
                        std::str::from_utf8(&old_config).unwrap(),
                        || false,
                    )
            } else {
                prepare_new_git(&fixture).apply(
                    &Fixture::config("v2.0.0"),
                    std::str::from_utf8(&old_config).unwrap(),
                    None,
                    || false,
                )
            };
            assert!(result.is_err());
            drop(lock);
            recover(&fixture.root).unwrap();
            assert_eq!(payload_snapshot(&fixture), before);
        }
    }

    #[test]
    fn incomplete_git_installation_recovers_the_installed_mirror() {
        let fixture = Fixture::new();
        fixture.install();
        let before = payload_snapshot(&fixture);
        let mut prepared = InstallTransaction::begin_git(
            &fixture.root,
            "sample",
            "sample.exe",
            &String::from_utf8(fixture.read("data/apps/sample/app.json")).unwrap(),
            || false,
        )
        .unwrap();
        fs::write(
            fixture
                .root
                .join("data/apps/sample/working")
                .join(ROOT_CONFIG_FILE),
            b"name: sample",
        )
        .unwrap();
        assert!(prepared
            .prepare_git_commit(
                ProgramFiles::new(
                    ["main.py", "pyappify.yml", "mid_lib/public/new.mid"]
                        .into_iter()
                        .map(str::to_string)
                )
                .unwrap()
            )
            .is_err());
        recover(&fixture.root).unwrap();
        assert_eq!(payload_snapshot(&fixture), before);
    }

    #[test]
    fn missing_old_mirror_manifest_blocks_replacement_and_keeps_unknown_files() {
        let fixture = Fixture::new();
        fixture.install();
        write_personal_data(&fixture);
        fs::remove_file(
            fixture
                .root
                .join("data/apps/sample/working")
                .join(MANIFEST_FILE),
        )
        .unwrap();
        let before = payload_snapshot(&fixture);
        assert!(fixture
            .prepare(&fixture.full("v2.0.0"), "v2.0.0", false)
            .is_err());
        recover(&fixture.root).unwrap();
        assert_eq!(payload_snapshot(&fixture), before);
        assert!(InstallTransaction::begin_git(
            &fixture.root,
            "sample",
            "sample.exe",
            &String::from_utf8(fixture.read("data/apps/sample/app.json")).unwrap(),
            || false
        )
        .is_err());
        assert_eq!(payload_snapshot(&fixture), before);
    }

    #[test]
    fn reopening_direct_git_install_restores_working_python_repo_and_mirror_metadata() {
        let fixture = Fixture::new();
        fixture.install();
        write_personal_data(&fixture);
        write_fixture_file(
            &fixture,
            "data/apps/sample/python/python.exe",
            b"old leftover Python",
        );
        write_fixture_file(
            &fixture,
            "data/apps/sample/repo/old.py",
            b"old leftover repository",
        );
        let before = payload_snapshot(&fixture);
        let _transaction = prepare_new_git(&fixture);
        assert_eq!(
            fixture.read("data/apps/sample/python/python.exe"),
            b"prepared Python"
        );
        assert!(recover(&fixture.root).unwrap());
        assert_eq!(payload_snapshot(&fixture), before);
        assert!(!recover(&fixture.root).unwrap());
    }

    #[test]
    fn cancelling_before_or_during_backups_never_removes_the_old_installation() {
        for checkpoint in 0..10 {
            let fixture = Fixture::new();
            fixture.install();
            let before = payload_snapshot(&fixture);
            let calls = Cell::new(0);
            let result = InstallTransaction::begin_git(
                &fixture.root,
                "sample",
                "sample.exe",
                &String::from_utf8(fixture.read("data/apps/sample/app.json")).unwrap(),
                || {
                    calls.set(calls.get() + 1);
                    calls.get() > checkpoint
                },
            );
            let _ = result;
            recover(&fixture.root).unwrap();
            assert_eq!(
                payload_snapshot(&fixture),
                before,
                "checkpoint={checkpoint}"
            );
        }
    }

    #[test]
    fn deleting_a_program_directory_keeps_user_files_inside_it() {
        let fixture = Fixture::new();
        fixture.install();
        write_fixture_file(
            &fixture,
            "data/apps/sample/working/removed/personal.notes",
            b"user notes",
        );
        let patch = fixture.zip("remove-directory.zip", "v2.0.0",
            Some(serde_json::json!({"modified":[PACKAGE_FILE], "deleted_dir":["data/apps/sample/working/removed"]})), &[]);
        fixture
            .apply(fixture.prepare(&patch, "v2.0.0", true).unwrap(), "v2.0.0")
            .unwrap();
        assert_eq!(
            fixture.read("data/apps/sample/working/removed/personal.notes"),
            b"user notes"
        );
        assert!(!fixture
            .root
            .join("data/apps/sample/working/removed/old.py")
            .exists());
    }

    #[test]
    fn empty_internal_directory_is_not_a_ready_frozen_installation() {
        let fixture = Fixture::new();
        fixture.install();
        fs::remove_file(
            fixture
                .root
                .join("data/apps/sample/working/_internal/python312.dll"),
        )
        .unwrap();
        assert!(!Fixture::package("v1.0.0")
            .payload_present(&fixture.root)
            .unwrap());
    }

    fn write_fixture_file(fixture: &Fixture, path: &str, contents: &[u8]) {
        let path = fixture.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    fn write_personal_data(fixture: &Fixture) {
        for (path, contents) in [
            ("configs/app.json", "preferences"),
            ("custom_chars/roles.json", "custom roles"),
            ("mid_lib/personal/song.mid", "personal MIDI"),
            ("mid_lib/.favorites.json", "favorites"),
            ("user/nested/prefs.json", "additional data"),
            ("mid_lib/public/old.mid", "old built-in"),
        ] {
            write_fixture_file(
                fixture,
                &format!("data/apps/sample/working/{path}"),
                contents.as_bytes(),
            );
        }
    }
    fn assert_personal_data(fixture: &Fixture) {
        for (path, contents) in [
            ("configs/app.json", "preferences"),
            ("custom_chars/roles.json", "custom roles"),
            ("mid_lib/personal/song.mid", "personal MIDI"),
            ("mid_lib/.favorites.json", "favorites"),
            ("user/nested/prefs.json", "additional data"),
        ] {
            assert_eq!(
                fixture.read(&format!("data/apps/sample/working/{path}")),
                contents.as_bytes()
            );
        }
    }
    fn install_old_git(fixture: &Fixture) {
        for (path, contents) in [
            ("pyappify.yml", "name: sample"),
            ("data/apps/sample/working/pyappify.yml", "name: sample"),
            ("data/apps/sample/working/main.py", "old Git program"),
            ("data/apps/sample/python/python.exe", "old Python"),
            ("data/apps/sample/python/Lib/old.pyd", "old Git dependency"),
            ("data/apps/sample/repo/main.py", "old repository"),
        ] {
            write_fixture_file(fixture, path, contents.as_bytes());
        }
        let repo_path = fixture.root.join("data/apps/sample/repo");
        fs::create_dir_all(repo_path.join("mid_lib/public")).unwrap();
        fs::write(repo_path.join("pyappify.yml"), "name: sample").unwrap();
        fs::write(repo_path.join("mid_lib/public/old.mid"), "old built-in").unwrap();
        let repository = git2::Repository::init(&repo_path).unwrap();
        let mut index = repository.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let commit = repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "old version",
                &tree,
                &[],
            )
            .unwrap();
        repository
            .tag_lightweight(
                "v1.0.0",
                &repository.find_object(commit, None).unwrap(),
                false,
            )
            .unwrap();
        let mut config: serde_json::Value =
            serde_json::from_slice(&Fixture::config("v1.0.0")).unwrap();
        config["update_source"] = serde_json::json!("git");
        fs::write(
            fixture.root.join("data/apps/sample/app.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        write_personal_data(fixture);
    }
    fn prepare_new_git(fixture: &Fixture) -> InstallTransaction {
        let mut prepared = InstallTransaction::begin_git(
            &fixture.root,
            "sample",
            "sample.exe",
            &String::from_utf8(fixture.read("data/apps/sample/app.json")).unwrap(),
            || false,
        )
        .unwrap();
        for (path, contents) in [
            (
                fixture
                    .root
                    .join("data/apps/sample/python")
                    .join("python.exe"),
                "prepared Python",
            ),
            (
                fixture.root.join("data/apps/sample/repo").join("main.py"),
                "new Git program",
            ),
            (
                fixture
                    .root
                    .join("data/apps/sample/working")
                    .join("main.py"),
                "new Git program",
            ),
            (
                fixture
                    .root
                    .join("data/apps/sample/working")
                    .join(ROOT_CONFIG_FILE),
                "name: sample",
            ),
            (
                fixture
                    .root
                    .join("data/apps/sample/working")
                    .join("mid_lib/public/new.mid"),
                "new Git MIDI",
            ),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        prepared
            .prepare_git_commit(
                ProgramFiles::new(
                    ["main.py", "pyappify.yml", "mid_lib/public/new.mid"]
                        .into_iter()
                        .map(str::to_string),
                )
                .unwrap(),
            )
            .unwrap();
        prepared
    }
    fn payload_snapshot(fixture: &Fixture) -> BTreeMap<String, Vec<u8>> {
        walkdir::WalkDir::new(&fixture.root)
            .into_iter()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                let name = entry
                    .path()
                    .strip_prefix(&fixture.root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if entry.file_type().is_file() && !name.starts_with(TASK_DIR) {
                    Some((name, fs::read(entry.path()).unwrap()))
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn frozen_full_replaces_runtime_and_public_songs_but_preserves_personal_data() {
        let fixture = Fixture::new();
        fixture.install();
        let working = fixture.root.join("data/apps/sample/working");
        for (path, content) in [
            ("configs/app.json", "preferences"),
            ("custom_chars/db.json", "characters"),
            ("mid_lib/personal/song.mid", "personal MIDI"),
            ("mid_lib/.favorites.json", "favorites"),
            ("mid_lib/public/obsolete.mid", "old bundled MIDI"),
            ("cache/openvino/model.blob", "compiled cache"),
        ] {
            fs::create_dir_all(working.join(path).parent().unwrap()).unwrap();
            fs::write(working.join(path), content).unwrap();
        }
        #[cfg(windows)]
        for path in [
            working.join("cache/openvino/model.blob"),
            fixture
                .root
                .join("data/apps/sample/working/_internal/library/native.pyd"),
        ] {
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(path, permissions).unwrap();
        }
        fixture
            .apply(
                fixture
                    .prepare(&fixture.full("v2.0.0"), "v2.0.0", false)
                    .unwrap(),
                "v2.0.0",
            )
            .unwrap();
        assert!(Fixture::package("v2.0.0")
            .payload_present(&fixture.root)
            .unwrap());
        assert!(!working.join("mid_lib/public/obsolete.mid").exists());
        for (path, content) in [
            ("configs/app.json", "preferences"),
            ("custom_chars/db.json", "characters"),
            ("mid_lib/personal/song.mid", "personal MIDI"),
            ("mid_lib/.favorites.json", "favorites"),
            ("cache/openvino/model.blob", "compiled cache"),
            ("mid_lib/public/new.mid", "bundled MIDI"),
        ] {
            assert_eq!(fs::read(working.join(path)).unwrap(), content.as_bytes());
        }
        assert_eq!(fixture.read("sample.exe"), b"current launcher");
        cleanup(&fixture.root).unwrap();
        assert!(!fs::read_dir(&fixture.root).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(TASK_DIR)));
        #[cfg(windows)]
        {
            let path = working.join("cache/openvino/model.blob");
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            assert!(permissions.readonly());
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[test]
    fn frozen_patch_updates_app_exe_and_builtins_without_touching_unchanged_runtime() {
        let fixture = Fixture::new();
        fixture
            .apply(
                fixture
                    .prepare(&fixture.full("v1.0.0"), "v1.0.0", false)
                    .unwrap(),
                "v1.0.0",
            )
            .unwrap();
        cleanup(&fixture.root).unwrap();
        let library = fixture
            .root
            .join("data/apps/sample/working/_internal/library/native.pyd");
        #[cfg(windows)]
        let _lock = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(&library)
                .unwrap()
        };
        let exe = "data/apps/sample/working/sample.exe";
        let song = "data/apps/sample/working/mid_lib/public/new.mid";
        let archive = fixture.zip_package(
            "patch-frozen.zip",
            &Fixture::package("v1.0.1"),
            Some(serde_json::json!({"modified":[PACKAGE_FILE, exe, song, "sample.exe"]})),
            &[
                (exe, b"updated frozen application"),
                (song, b"updated bundled MIDI"),
                ("sample.exe", b"ignored launcher"),
            ],
        );
        fixture
            .apply(fixture.prepare(&archive, "v1.0.1", true).unwrap(), "v1.0.1")
            .unwrap();
        assert_eq!(fixture.read(exe), b"updated frozen application");
        assert_eq!(fixture.read(song), b"updated bundled MIDI");
        assert_eq!(fs::read(library).unwrap(), b"library unchanged");
        assert_eq!(fixture.read("sample.exe"), b"current launcher");
    }

    #[test]
    fn frozen_full_cancel_after_carrying_favorites_restores_runtime_and_data() {
        let fixture = Fixture::new();
        fixture.install();
        let favorites = fixture
            .root
            .join("data/apps/sample/working/mid_lib/.favorites.json");
        fs::create_dir_all(favorites.parent().unwrap()).unwrap();
        fs::write(&favorites, b"favorite songs").unwrap();
        let prepared = fixture
            .prepare(&fixture.full("v2.0.0"), "v2.0.0", false)
            .unwrap();
        let saw_fresh_directory = Cell::new(false);
        let old = fs::read_to_string(fixture.root.join("data/apps/sample/app.json")).unwrap();
        assert!(prepared
            .apply(&Fixture::config("v2.0.0"), &old, || {
                if !favorites.exists() {
                    saw_fresh_directory.set(true);
                }
                saw_fresh_directory.get() && favorites.exists()
            })
            .is_err());
        assert_eq!(fs::read(&favorites).unwrap(), b"favorite songs");
        assert_eq!(
            fixture.read("data/apps/sample/working/README.md"),
            b"old main"
        );
        assert_eq!(
            fixture.read("data/apps/sample/working/sample.exe"),
            b"frozen application"
        );
        assert_eq!(
            read_package(&fixture.root).unwrap().unwrap().runtime,
            Fixture::package("v1.0.0").runtime
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("data/apps/sample/app.json")).unwrap(),
            old
        );
        assert!(!recover(&fixture.root).unwrap());
    }

    #[test]
    fn changing_frozen_entry_requires_a_full_package() {
        let fixture = Fixture::new();
        fixture.install();
        let mut next = Fixture::package("v2.0.0");
        next.runtime = Runtime::Pyinstaller {
            executable: "updated.exe".into(),
        };
        let archive = fixture.zip_package(
            "runtime-patch.zip",
            &next,
            Some(serde_json::json!({"modified":[PACKAGE_FILE]})),
            &[],
        );
        assert!(fixture
            .prepare(&archive, "v2.0.0", true)
            .err()
            .unwrap()
            .downcast_ref::<NeedsFullPackage>()
            .is_some());
        assert_eq!(
            fixture.read("data/apps/sample/working/README.md"),
            b"old main"
        );
    }

    #[test]
    fn bundled_midi_cannot_remove_personal_container_or_escape_paths() {
        let package = Fixture::package("v1.0.0");
        assert!(package.allows("data/apps/sample/working/mid_lib/public/new.mid", false));
        assert!(!package.allows("data/apps/sample/working/mid_lib/.favorites.json", false));
        assert!(!package.allows("data/apps/sample/working/../../private.txt", false));
        assert!(package.protects_working_path("mid_lib"));
        assert!(!package.protects_working_path("mid_lib/public"));
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn complete_zip_replaces_old_trees_and_preserves_user_data_and_preferences() {
        let fixture = Fixture::new();
        fixture.install();
        fs::write(
            fixture
                .root
                .join("data/apps/sample/working/_internal/stale.pyd"),
            b"stale dependency",
        )
        .unwrap();
        fs::write(
            fixture.root.join("data/apps/sample/working/stale.py"),
            b"stale source",
        )
        .unwrap();
        for preserve in Fixture::package("v1.0.0").preserve_paths {
            let directory = fixture.root.join("data/apps/sample/working").join(preserve);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("user.json"), b"user data").unwrap();
        }
        let archive = fixture.full("v2.0.0");
        fixture
            .apply(
                fixture.prepare(&archive, "v2.0.0", false).unwrap(),
                "v2.0.0",
            )
            .unwrap();
        assert!(!fixture
            .root
            .join("data/apps/sample/working/_internal/stale.pyd")
            .exists());
        // A new file that was never in the shipped manifest is user data.
        assert_eq!(
            fixture.read("data/apps/sample/working/stale.py"),
            b"stale source"
        );
        for preserve in Fixture::package("v2.0.0").preserve_paths {
            assert_eq!(
                fixture.read(&format!("data/apps/sample/working/{preserve}/user.json")),
                b"user data"
            );
        }
        let config: serde_json::Value =
            serde_json::from_slice(&fixture.read("data/apps/sample/app.json")).unwrap();
        assert_eq!(config["auto_start"], true);
        assert_eq!(config["update_method"], "MANUAL_UPDATE");
        assert_eq!(config["current_profile"], "China");
        assert_eq!(fixture.read("sample.exe"), b"current launcher");
        assert_eq!(
            baseline(&fixture.root, "sample")
                .unwrap()
                .unwrap()
                .package
                .version,
            "v2.0.0"
        );
    }

    #[test]
    fn patch_applies_all_categories_without_touching_locked_unchanged_library() {
        let fixture = Fixture::new();
        fixture.install();
        let mut open = fs::OpenOptions::new();
        open.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            open.share_mode(1);
        }
        let _locked = open
            .open(
                fixture
                    .root
                    .join("data/apps/sample/working/_internal/library/native.pyd"),
            )
            .unwrap();
        let patch = fixture.zip(
            "patch.zip",
            "v3.0.0",
            Some(serde_json::json!({
                "modified":[PACKAGE_FILE,"data/apps/sample/working/README.md"],
                "added":["data/apps/sample/working/new/a.py"],
                "deleted":["data/apps/sample/working/removed/old.py"],
                "added_dir":["data/apps/sample/working/new","data/apps/sample/working/empty"],
                "deleted_dir":["data/apps/sample/working/removed"]
            })),
            &[
                ("data/apps/sample/working/README.md", b"new main"),
                ("data/apps/sample/working/new/a.py", b"new file"),
            ],
        );
        fixture
            .apply(fixture.prepare(&patch, "v3.0.0", true).unwrap(), "v3.0.0")
            .unwrap();
        assert_eq!(
            fixture.read("data/apps/sample/working/README.md"),
            b"new main"
        );
        assert_eq!(
            fixture.read("data/apps/sample/working/new/a.py"),
            b"new file"
        );
        assert!(fixture.root.join("data/apps/sample/working/empty").is_dir());
        assert!(!fixture
            .root
            .join("data/apps/sample/working/removed")
            .exists());
        assert_eq!(
            fixture.read("data/apps/sample/working/_internal/library/native.pyd"),
            b"library unchanged"
        );
    }

    #[test]
    fn missing_categories_and_empty_changes_are_supported() {
        let fixture = Fixture::new();
        fixture.install();
        let patch = fixture.zip(
            "patch.zip",
            "v2.0.0",
            Some(serde_json::json!({"modified":[PACKAGE_FILE]})),
            &[],
        );
        fixture
            .apply(fixture.prepare(&patch, "v2.0.0", true).unwrap(), "v2.0.0")
            .unwrap();
        assert_eq!(
            fixture.read("data/apps/sample/working/README.md"),
            b"old main"
        );
    }

    #[test]
    fn untrusted_or_missing_baseline_file_requires_complete_zip() {
        let fixture = Fixture::new();
        fixture.install();
        let patch = fixture.zip(
            "patch.zip",
            "v2.0.0",
            Some(
                serde_json::json!({"modified":[PACKAGE_FILE,"data/apps/sample/working/README.md"]}),
            ),
            &[("data/apps/sample/working/README.md", b"new main")],
        );
        assert!(fixture
            .prepare(&patch, "v2.0.0", false)
            .err()
            .unwrap()
            .is::<NeedsFullPackage>());
        fs::remove_file(fixture.root.join("data/apps/sample/working/README.md")).unwrap();
        assert!(fixture
            .prepare(&patch, "v2.0.0", true)
            .err()
            .unwrap()
            .is::<NeedsFullPackage>());
        assert!(!fixture.root.join(TASK_DIR).exists());
    }

    #[test]
    fn paths_outside_payload_and_windows_aliases_are_rejected_before_extraction() {
        for bad in [
            "../escape",
            "/escape",
            "data/apps/other/working/x",
            "data/apps/sample/app.json",
            "data/apps/sample/working/configs/user.json",
            "data/apps/sample/working/CON.txt",
            "data/apps/sample/working/x:stream",
            "data/apps/sample/working/x. ",
            "data\\apps\\sample\\working\\x",
        ] {
            let fixture = Fixture::new();
            let archive = fixture.zip("bad.zip", "v2.0.0", None, &[(bad, b"bad")]);
            assert!(fixture.prepare(&archive, "v2.0.0", false).is_err(), "{bad}");
            assert!(!fixture.root.join(TASK_DIR).exists());
        }
    }

    #[test]
    fn inconsistent_parent_casing_is_rejected() {
        let fixture = Fixture::new();
        let archive = fixture.zip(
            "bad.zip",
            "v2.0.0",
            None,
            &[
                ("data/apps/sample/working/Src/a.py", b"a"),
                ("data/apps/sample/working/src/b.py", b"b"),
            ],
        );
        assert!(fixture.prepare(&archive, "v2.0.0", false).is_err());
    }

    #[test]
    fn invalid_deletion_or_missing_payload_never_modifies_the_install() {
        let fixture = Fixture::new();
        fixture.install();
        for changes in [
            serde_json::json!({"modified":[PACKAGE_FILE,"data/apps/sample/working/README.md"]}),
            serde_json::json!({"modified":[PACKAGE_FILE],"deleted_dir":["data/apps/sample/working"]}),
            serde_json::json!({"modified":[PACKAGE_FILE],"deleted_dir":["data/apps/sample/working/_INTERNAL"]}),
            serde_json::json!({"modified":[PACKAGE_FILE],"deleted_dir":["data/apps/sample/working/configs"]}),
            serde_json::json!({"modified":[PACKAGE_FILE],"deleted_dir":["data/apps/sample/working/user"]}),
        ] {
            let archive = fixture.zip("bad.zip", "v2.0.0", Some(changes), &[]);
            assert!(fixture.prepare(&archive, "v2.0.0", true).is_err());
            assert_eq!(
                fixture.read("data/apps/sample/working/README.md"),
                b"old main"
            );
        }
        let archive = fixture.zip(
            "invalid-runtime.zip",
            "v2.0.0",
            None,
            &[
                (ROOT_CONFIG_FILE, b"name: sample"),
                ("data/apps/sample/working/pyappify.yml", b"name: sample"),
                ("data/apps/sample/working/sample.exe", b"frozen application"),
                (
                    "data/apps/sample/working/_internal",
                    b"not a runtime directory",
                ),
            ],
        );
        assert!(fixture.prepare(&archive, "v2.0.0", false).is_err());
        assert!(!fixture.root.join(TASK_DIR).exists());
        assert_eq!(
            read_package(&fixture.root).unwrap().unwrap().version,
            "v1.0.0"
        );
    }

    #[test]
    fn cancellation_during_extraction_removes_only_staging() {
        let fixture = Fixture::new();
        fixture.install();
        let calls = Cell::new(0);
        let result = prepare(
            &fixture.full("v2.0.0"),
            &fixture.root,
            &Expected {
                app_name: "sample",
                resource_id: "sample",
                version: "v2.0.0",
                launcher: "sample.exe",
            },
            false,
            || {
                calls.set(calls.get() + 1);
                calls.get() > 3
            },
        );
        assert!(result.is_err());
        assert!(!fixture.root.join(TASK_DIR).exists());
        assert_eq!(
            fixture.read("data/apps/sample/working/README.md"),
            b"old main"
        );
    }

    #[test]
    fn cancellation_mid_apply_restores_files_config_and_baseline() {
        let fixture = Fixture::new();
        fixture.install();
        let old = fixture.read("data/apps/sample/app.json");
        let prepared = fixture
            .prepare(&fixture.full("v2.0.0"), "v2.0.0", false)
            .unwrap();
        let calls = Cell::new(0);
        assert!(prepared
            .apply(
                &Fixture::config("v2.0.0"),
                std::str::from_utf8(&old).unwrap(),
                || {
                    calls.set(calls.get() + 1);
                    calls.get() > 4
                }
            )
            .is_err());
        assert_eq!(fixture.read("data/apps/sample/app.json"), old);
        assert_eq!(
            baseline(&fixture.root, "sample")
                .unwrap()
                .unwrap()
                .package
                .version,
            "v1.0.0"
        );
        assert_eq!(
            fixture.read("data/apps/sample/working/README.md"),
            b"old main"
        );
        assert!(!fixture.root.join(TASK_DIR).exists());
    }

    #[test]
    fn startup_recovers_an_interrupted_move_before_publishing_new_version() {
        let fixture = Fixture::new();
        fixture.install();
        let prepared = fixture
            .prepare(&fixture.full("v2.0.0"), "v2.0.0", false)
            .unwrap();
        let journal = Journal {
            package: Some(prepared.package),
            app_name: "sample".into(),
            launcher: "sample.exe".into(),
            actions: prepared.transaction.actions,
            rollback_config: Some(
                String::from_utf8(fixture.read("data/apps/sample/app.json")).unwrap(),
            ),
        };
        durable_write(
            &fixture.root.join(TASK_DIR).join("journal.json"),
            &serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        let config_backup = journal
            .actions
            .iter()
            .position(
                |action| matches!(action, Action::Move { from, .. } if from == ROOT_CONFIG_FILE),
            )
            .unwrap();
        for action in journal.actions.iter().take(config_backup + 1) {
            if let Action::Move { from, to } = action {
                let to = fixture.root.join(to);
                fs::create_dir_all(to.parent().unwrap()).unwrap();
                fs::rename(fixture.root.join(from), to).unwrap();
            }
        }
        assert!(!fixture.root.join(ROOT_CONFIG_FILE).exists());
        assert!(recover(&fixture.root).unwrap());
        assert_eq!(fixture.read(ROOT_CONFIG_FILE), b"name: sample");
        assert_eq!(
            fixture.read("data/apps/sample/working/_internal/library/native.pyd"),
            b"library unchanged"
        );
        assert_eq!(
            read_package(&fixture.root).unwrap().unwrap().version,
            "v1.0.0"
        );
        assert!(!recover(&fixture.root).unwrap());
    }

    #[test]
    fn patches_support_file_directory_type_changes() {
        let fixture = Fixture::new();
        fixture.install();
        let patch = fixture.zip("patch.zip","v2.0.0",Some(serde_json::json!({
            "modified":[PACKAGE_FILE],"deleted":["data/apps/sample/working/README.md"],"deleted_dir":["data/apps/sample/working/removed"],
            "added_dir":["data/apps/sample/working/README.md"],"added":["data/apps/sample/working/README.md/a","data/apps/sample/working/removed"]
        })),&[("data/apps/sample/working/README.md/a",b"nested"),("data/apps/sample/working/removed",b"now a file")]);
        fixture
            .apply(fixture.prepare(&patch, "v2.0.0", true).unwrap(), "v2.0.0")
            .unwrap();
        assert_eq!(
            fixture.read("data/apps/sample/working/README.md/a"),
            b"nested"
        );
        assert_eq!(
            fixture.read("data/apps/sample/working/removed"),
            b"now a file"
        );
    }

    #[cfg(windows)]
    #[test]
    fn locked_changed_file_rolls_back_earlier_moves() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        fixture.install();
        let _locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(fixture.root.join("data/apps/sample/working/README.md"))
            .unwrap();
        let patch = fixture.zip(
            "patch.zip",
            "v2.0.0",
            Some(
                serde_json::json!({"modified":[PACKAGE_FILE,"data/apps/sample/working/README.md"]}),
            ),
            &[("data/apps/sample/working/README.md", b"new main")],
        );
        assert!(fixture
            .apply(fixture.prepare(&patch, "v2.0.0", true).unwrap(), "v2.0.0")
            .is_err());
        assert_eq!(
            fixture.read("data/apps/sample/working/README.md"),
            b"old main"
        );
        assert_eq!(
            read_package(&fixture.root).unwrap().unwrap().version,
            "v1.0.0"
        );
        assert!(!fixture.root.join(TASK_DIR).exists());
    }

    #[test]
    #[ignore = "subprocess fixture for the running-launcher preservation test"]
    fn hold_child() {
        if std::env::var_os("PYAPPIFY_TEST_HOLD").is_some() {
            std::io::stdin().read_exact(&mut [0u8; 1]).unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn complete_and_incremental_updates_keep_the_running_launcher_untouched() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        let fixture = Fixture::new();
        fixture.install();
        fs::copy(
            std::env::current_exe().unwrap(),
            fixture.root.join("sample.exe"),
        )
        .unwrap();
        let original = fixture.read("sample.exe");
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(fixture.root.join("sample.exe"))
            .unwrap();
        let mut child = Command::new(fixture.root.join("sample.exe"))
            .args(["--ignored", "--exact", "mirror_zip::tests::hold_child"])
            .env("PYAPPIFY_TEST_HOLD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        // Wait for the image mapping, while the child waits on its input pipe.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let result = fixture.apply(
            fixture
                .prepare(&fixture.full("v2.0.0"), "v2.0.0", false)
                .unwrap(),
            "v2.0.0",
        );
        let alive = child.try_wait().unwrap().is_none();
        let patch = fixture.zip("launcher.zip", "v3.0.0", Some(serde_json::json!({"modified":[PACKAGE_FILE,"sample.exe","data/apps/sample/working/sample.exe"],"added":["data/apps/sample/working/_internal/library/added.pyd","data/apps/sample/working/helper.exe"]})), &[("sample.exe",b"ignored launcher"),("data/apps/sample/working/sample.exe",b"updated application"),("data/apps/sample/working/_internal/library/added.pyd",b"added library"),("data/apps/sample/working/helper.exe",b"helper")]);
        let patch_result =
            fixture.apply(fixture.prepare(&patch, "v3.0.0", true).unwrap(), "v3.0.0");
        let ignored_delete = fixture.zip(
            "launcher-delete.zip",
            "v4.0.0",
            Some(serde_json::json!({"modified":[PACKAGE_FILE],"deleted":["sample.exe"]})),
            &[],
        );
        let delete_result = fixture.apply(
            fixture.prepare(&ignored_delete, "v4.0.0", true).unwrap(),
            "v4.0.0",
        );
        let still_alive = child.try_wait().unwrap().is_none();
        child.stdin.take().unwrap().write_all(b"x").unwrap();
        child.wait().unwrap();
        result.unwrap();
        patch_result.unwrap();
        delete_result.unwrap();
        assert!(alive && still_alive);
        assert_eq!(fixture.read("sample.exe"), original);
        assert_eq!(
            fixture.read("data/apps/sample/working/sample.exe"),
            b"updated application"
        );
        assert_eq!(
            fixture.read("data/apps/sample/working/_internal/library/added.pyd"),
            b"added library"
        );
        assert_eq!(
            fixture.read("data/apps/sample/working/helper.exe"),
            b"helper"
        );
        drop(locked);
        cleanup(&fixture.root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn late_lock_after_rm_preflight_rolls_back_earlier_deletions() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        fixture.install();
        let patch = fixture.zip("late-lock.zip", "v2.0.0", Some(serde_json::json!({"modified":[PACKAGE_FILE,"data/apps/sample/working/README.md"],"deleted_dir":["data/apps/sample/working/removed"]})), &[("data/apps/sample/working/README.md",b"new main")]);
        let prepared = fixture.prepare(&patch, "v2.0.0", true).unwrap();
        let files = crate::restart_manager::existing_files(
            &prepared.replacement_paths().unwrap(),
            true,
            || false,
        )
        .unwrap();
        let mut session = crate::restart_manager::Session::new(&files, &[], &[], || false).unwrap();
        session.shutdown(false, || false).unwrap();
        // A new lock can appear after RM. The first action deletes a directory;
        // the next action fails and must restore that earlier deletion.
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(fixture.root.join("data/apps/sample/working/README.md"))
            .unwrap();
        assert!(fixture.apply(prepared, "v2.0.0").is_err());
        session.restart().unwrap();
        assert_eq!(
            fixture.read("data/apps/sample/working/removed/old.py"),
            b"old file"
        );
        assert_eq!(
            fixture.read("data/apps/sample/working/README.md"),
            b"old main"
        );
        assert_eq!(
            read_package(&fixture.root).unwrap().unwrap().version,
            "v1.0.0"
        );
        drop(locked);
    }
}
