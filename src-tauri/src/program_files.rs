//! Program paths come from a clean body build or the installed Git revision.
//! Everything else in working is a user file; no content or timestamp checks.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fs, path::Path};

pub const MANIFEST_FILE: &str = "pyappify-files.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProgramFiles {
    pub format: u32,
    pub files: Vec<String>,
}

pub fn key(path: &str) -> String {
    path.replace('\\', "/").to_ascii_lowercase()
}

pub fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(['\\', ':', '\0'])
        || path.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.ends_with(['.', ' '])
                || part.chars().any(|c| c < ' ' || "<>\"|?*".contains(c))
                || matches!(
                    part.split('.')
                        .next()
                        .unwrap_or("")
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
        })
    {
        bail!("Invalid program file path: {path}");
    }
    Ok(())
}

pub fn reject_link(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("Program and user files cannot contain links or junctions");
        }
    }
    if metadata.file_type().is_symlink() {
        bail!("Program and user files cannot contain links");
    }
    Ok(())
}

impl ProgramFiles {
    pub fn new(files: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut files: Vec<_> = files.into_iter().collect();
        files.sort_by_key(|path| key(path));
        let mut seen = BTreeSet::new();
        for path in &files {
            validate_path(path)?;
            if !seen.insert(key(path)) {
                bail!("Duplicate program file path: {path}");
            }
        }
        Ok(Self { format: 1, files })
    }

    pub fn read(body: &Path) -> Result<Self> {
        reject_link(body)?;
        let path = body.join(MANIFEST_FILE);
        reject_link(&path)?;
        if fs::metadata(&path)?.len() > 16 * 1024 * 1024 {
            bail!("Application program manifest is too large");
        }
        let record: Self = serde_json::from_slice(&fs::read(path)?)?;
        if record.format != 1 || !record.files.iter().any(|p| p == MANIFEST_FILE) {
            bail!("Unsupported application program manifest");
        }
        Self::new(record.files)
    }

    /// Called only on the completed build, before running it or moving user files.
    pub fn capture_clean_body(body: &Path) -> Result<Self> {
        let mut files = vec![MANIFEST_FILE.to_string()];
        for entry in walkdir::WalkDir::new(body).follow_links(false) {
            let entry = entry?;
            reject_link(entry.path())?;
            if entry.file_type().is_file() {
                let name = entry
                    .path()
                    .strip_prefix(body)?
                    .to_str()
                    .context("Non-UTF8 application body path")?
                    .replace('\\', "/");
                if name != MANIFEST_FILE {
                    files.push(name);
                }
            }
        }
        let record = Self::new(files)?;
        fs::write(
            body.join(MANIFEST_FILE),
            serde_json::to_vec_pretty(&record)?,
        )?;
        Ok(record)
    }

    pub fn git(repo: &Path, revision: Option<&str>) -> Result<Self> {
        let repository = git2::Repository::open(repo)?;
        let tree = match revision {
            Some(revision) => repository.revparse_single(revision)?.peel_to_tree()?,
            None => repository.head()?.peel_to_tree()?,
        };
        let mut files = Vec::new();
        collect_git_files(&repository, &tree, "", &mut files)?;
        Self::new(files)
    }

    pub fn keys(&self) -> BTreeSet<String> {
        self.files.iter().map(|path| key(path)).collect()
    }
}

fn collect_git_files(
    repository: &git2::Repository,
    tree: &git2::Tree<'_>,
    prefix: &str,
    files: &mut Vec<String>,
) -> Result<()> {
    for entry in tree.iter() {
        let path = format!("{prefix}{}", entry.name()?);
        match entry.kind() {
            Some(git2::ObjectType::Blob) => {
                if entry.filemode() == 0o120000 {
                    bail!("Git application contains a symbolic link: {path}");
                }
                files.push(path);
            }
            Some(git2::ObjectType::Tree) => {
                collect_git_files(
                    repository,
                    &repository.find_tree(entry.id())?,
                    &format!("{path}/"),
                    files,
                )?;
            }
            Some(git2::ObjectType::Commit) => {
                let submodule = git2::Repository::open(
                    repository
                        .workdir()
                        .context("Missing Git working directory")?
                        .join(&path),
                )?;
                let subtree = submodule.find_commit(entry.id())?.tree()?;
                let mut subfiles = Vec::new();
                collect_git_files(&submodule, &subtree, "", &mut subfiles)?;
                files.extend(subfiles.into_iter().map(|name| format!("{path}/{name}")));
            }
            _ => bail!("Unsupported Git program path: {path}"),
        }
    }
    Ok(())
}

/// Read the committed route/version, not the currently selected source.
pub fn installed(root: &Path, app_name: &str) -> Result<Option<ProgramFiles>> {
    let base = root.join("data/apps").join(app_name);
    let working = base.join("working");
    if !working.is_dir() || fs::read_dir(&working)?.next().is_none() {
        return Ok(None);
    }
    let state: serde_json::Value = if base.join("app.json").is_file() {
        serde_json::from_slice(&fs::read(base.join("app.json"))?)?
    } else {
        serde_json::Value::Null
    };
    let installed = state
        .get("installation")
        .filter(|value| value.is_object())
        .unwrap_or(&state);
    let source = installed
        .get("source")
        .or_else(|| installed.get("update_source"))
        .and_then(serde_json::Value::as_str);
    if source == Some("git")
        || (source.is_none()
            && base.join("repo/.git").exists()
            && !root.join("pyappify-release.json").is_file())
    {
        let version = installed
            .get("version")
            .or_else(|| installed.get("current_version"))
            .and_then(serde_json::Value::as_str);
        return Ok(Some(ProgramFiles::git(&base.join("repo"), version)?));
    }
    if !working.join(MANIFEST_FILE).is_file() {
        bail!("The installed Mirror package has no pyappify-files.json. Its program files cannot be identified reliably; the old installation and user files have been kept. Use a clean package of that exact version to recover its manifest before switching or replacing it.");
    }
    Ok(Some(ProgramFiles::read(&working)?))
}

pub fn migrate_users(
    old: &Path,
    new: &Path,
    old_program: &ProgramFiles,
    new_program: &ProgramFiles,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    let old_keys = old_program.keys();
    let new_keys = new_program.keys();
    let mut new_directories = BTreeSet::new();
    for name in &new_keys {
        let mut parent = name.as_str();
        while let Some((path, _)) = parent.rsplit_once('/') {
            new_directories.insert(path.to_string());
            parent = path;
        }
    }
    for entry in walkdir::WalkDir::new(old)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let value = entry.path().strip_prefix(old).unwrap_or(Path::new(""));
            !value.components().next().is_some_and(|part| {
                matches!(
                    part.as_os_str()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .as_str(),
                    "_internal" | ".git"
                )
            })
        })
    {
        if cancelled() {
            bail!("Operation cancelled by user");
        }
        let entry = entry?;
        reject_link(entry.path())?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry
            .path()
            .strip_prefix(old)?
            .to_str()
            .context("Non-UTF8 user file path")?
            .replace('\\', "/");
        validate_path(&name)?;
        let normalized = key(&name);
        if old_keys.contains(&normalized)
            || new_keys.contains(&normalized)
            || new_directories.contains(&normalized)
            || normalized == ".pip_update_needed.tmp"
        {
            continue;
        }
        let mut ancestor = normalized.as_str();
        let mut collision = false;
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            if new_keys.contains(parent) {
                collision = true;
                break;
            }
            ancestor = parent;
        }
        let target = new.join(&name);
        if collision || target.exists() {
            continue;
        }
        fs::create_dir_all(target.parent().unwrap())?;
        fs::copy(entry.path(), target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/program-files-tests")
                .join(format!("{:x}", rand::random::<u64>()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn write(&self, name: &str, bytes: &[u8]) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn clean_build_manifest_includes_every_external_resource_and_itself() {
        let fixture = Fixture::new();
        for name in [
            "sample.exe",
            "_internal/python312.dll",
            "assets/model.bin",
            "unexpected/resources/song.mid",
        ] {
            fixture.write(name, b"program");
        }
        let manifest = ProgramFiles::capture_clean_body(&fixture.0).unwrap();
        assert_eq!(manifest.files.len(), 5);
        assert!(manifest
            .files
            .contains(&"unexpected/resources/song.mid".into()));
        assert_eq!(manifest, ProgramFiles::read(&fixture.0).unwrap());
        assert!(manifest.files.iter().all(|path| !path.starts_with("data/")));
    }

    #[test]
    fn user_migration_uses_program_paths_and_new_program_wins_collisions() {
        let fixture = Fixture::new();
        fixture.write("old/main.py", b"old program");
        fixture.write("old/_internal/python312.dll", b"old runtime");
        let old = ProgramFiles::capture_clean_body(&fixture.0.join("old")).unwrap();
        for (name, content) in [
            ("old/main.py", b"edited program".as_slice()),
            ("old/arbitrary/deep/personal.json", b"preferences"),
            ("old/new-program.dat", b"user collision"),
            ("old/new-folder", b"user file conflicts with directory"),
            (
                "old/new-file/personal.json",
                b"user directory conflicts with file",
            ),
            ("old/_internal/generated.dll", b"runtime generated file"),
            ("new/main.py", b"new program"),
            ("new/new-program.dat", b"new program"),
            ("new/new-folder/data", b"new program"),
            ("new/new-file", b"new program"),
        ] {
            fixture.write(name, content);
        }
        let new = ProgramFiles::capture_clean_body(&fixture.0.join("new")).unwrap();
        migrate_users(
            &fixture.0.join("old"),
            &fixture.0.join("new"),
            &old,
            &new,
            || false,
        )
        .unwrap();
        assert_eq!(
            fs::read(fixture.0.join("new/arbitrary/deep/personal.json")).unwrap(),
            b"preferences"
        );
        for name in ["main.py", "new-program.dat", "new-file", "new-folder/data"] {
            assert_eq!(
                fs::read(fixture.0.join("new").join(name)).unwrap(),
                b"new program"
            );
        }
        assert!(!fixture.0.join("new/_internal/generated.dll").exists());
        assert_eq!(
            fs::read(fixture.0.join("old/main.py")).unwrap(),
            b"edited program"
        );
    }

    #[test]
    fn git_program_paths_use_the_installed_revision() {
        let fixture = Fixture::new();
        fixture.write("repo/main.py", b"old program");
        fixture.write("repo/removed.py", b"old removed program");
        let repository = git2::Repository::init(fixture.0.join("repo")).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let mut index = repository.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let old_commit = repository
            .commit(Some("HEAD"), &signature, &signature, "old", &tree, &[])
            .unwrap();
        repository
            .tag_lightweight(
                "v1",
                &repository.find_object(old_commit, None).unwrap(),
                false,
            )
            .unwrap();
        fs::remove_file(fixture.0.join("repo/removed.py")).unwrap();
        index.remove_path(Path::new("removed.py")).unwrap();
        fixture.write("repo/main.py", b"new program");
        fixture.write("repo/added/file.dat", b"new asset");
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "new",
                &tree,
                &[&repository.find_commit(old_commit).unwrap()],
            )
            .unwrap();
        let old = ProgramFiles::git(&fixture.0.join("repo"), Some("v1")).unwrap();
        let new = ProgramFiles::git(&fixture.0.join("repo"), None).unwrap();
        assert!(old.files.contains(&"removed.py".into()));
        assert!(!old.files.contains(&"added/file.dat".into()));
        assert!(!new.files.contains(&"removed.py".into()));
        assert!(new.files.contains(&"added/file.dat".into()));
    }
}
