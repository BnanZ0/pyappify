//! Program paths come from a clean body build or the installed Git revision.
//! Everything else in working is a user file; no content or timestamp checks.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fs, path::Path};

pub const MANIFEST_FILE: &str = "pyappify-files.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProgramFiles {
    pub format: u32,
    pub version: String,
    pub executable: String,
    pub files: Vec<String>,
}

pub(crate) use crate::extensions::file_operations::key;

pub fn validate_path(path: &str) -> Result<()> {
    crate::extensions::file_operations::relative(path).map(|_| ())
}

pub fn reject_link(path: &Path) -> Result<()> {
    if crate::extensions::file_operations::is_reparse(&fs::symlink_metadata(path)?) {
        bail!("Program and user files cannot contain links or junctions");
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
        Ok(Self {
            format: 1,
            version: String::new(),
            executable: String::new(),
            files,
        })
    }

    pub fn read(body: &Path) -> Result<Self> {
        reject_link(body)?;
        let path = body.join(MANIFEST_FILE);
        reject_link(&path)?;
        if fs::metadata(&path)?.len() > 16 * 1024 * 1024 {
            bail!("Application program manifest is too large");
        }
        Self::from_reader(fs::File::open(path)?)
    }

    pub(crate) fn from_reader(reader: impl std::io::Read) -> Result<Self> {
        let record: Self = super::package::read_json(reader, 16 * 1024 * 1024)
            .context("Invalid pyappify-files.json")?;
        record.validate_body()?;
        let mut normalized = Self::new(record.files)?;
        normalized.executable = record.executable;
        normalized.version = record.version;
        Ok(normalized)
    }

    fn validate_body(&self) -> Result<()> {
        if self.format != 1 || !self.files.iter().any(|p| p == MANIFEST_FILE) {
            bail!("Unsupported application program manifest");
        }
        if self.version.is_empty() {
            bail!("Missing version in pyappify-files.json");
        }
        validate_path(&self.executable)?;
        if self.executable.contains('/')
            || !self.executable.to_ascii_lowercase().ends_with(".exe")
            || !self.files.iter().any(|p| key(p) == key(&self.executable))
        {
            bail!("Invalid or unlisted executable in pyappify-files.json");
        }
        Ok(())
    }

    /// Called only on the completed build, before running it or moving user files.
    pub fn capture_clean_body(body: &Path, version: &str, executable: &str) -> Result<Self> {
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
        let mut record = Self::new(files)?;
        record.version = version.into();
        record.executable = executable.into();
        record.validate_body()?;
        fs::write(
            body.join(MANIFEST_FILE),
            serde_json::to_vec_pretty(&record)?,
        )?;
        Ok(record)
    }

    pub fn git(repo: &Path, revision: Option<&str>) -> Result<Self> {
        let repository = crate::git::open_repository(repo)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_manifest_requires_its_own_version() {
        let missing =
            br#"{"format":1,"executable":"app.exe","files":["pyappify-files.json","app.exe"]}"#;
        assert!(ProgramFiles::from_reader(missing.as_slice()).is_err());
        let empty = br#"{"format":1,"version":"","executable":"app.exe","files":["pyappify-files.json","app.exe"]}"#;
        assert!(ProgramFiles::from_reader(empty.as_slice()).is_err());
        assert!(validate_path("COM¹.txt").is_err());
        assert!(ProgramFiles::new(["Ä.dat".into(), "ä.dat".into()]).is_err());
    }

    #[tokio::test]
    async fn bare_git_program_inventory_excludes_submodule_metadata_from_user_migration() {
        let root = std::env::temp_dir().join(format!("pyappify-program-{:x}", rand::random::<u64>()));
        let module_path = root.join("module");
        fs::create_dir_all(&module_path).unwrap();
        let module = git2::Repository::init(&module_path).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        {
            fs::write(module_path.join("template.txt"), "program asset").unwrap();
            let mut index = module.index().unwrap();
            index.add_path(Path::new("template.txt")).unwrap();
            index.write().unwrap();
            let tree = module.find_tree(index.write_tree().unwrap()).unwrap();
            module
                .commit(Some("HEAD"), &signature, &signature, "asset", &tree, &[])
                .unwrap();
        }

        let source_path = root.join("source");
        let source = git2::Repository::init(&source_path).unwrap();
        {
            let mut child = source
                .submodule(module_path.to_str().unwrap(), Path::new("templates"), true)
                .unwrap();
            drop(child.clone(None).unwrap());
            child.add_finalize().unwrap();
            let tree = source.find_tree(source.index().unwrap().write_tree().unwrap()).unwrap();
            let commit = source
                .commit(Some("HEAD"), &signature, &signature, "body", &tree, &[])
                .unwrap();
            source
                .tag_lightweight("v1.0.0", &source.find_object(commit, None).unwrap(), false)
                .unwrap();
        }
        let repo_path = root.join("repo");
        drop(
            git2::build::RepoBuilder::new()
                .bare(true)
                .clone(source_path.to_str().unwrap(), &repo_path)
                .unwrap(),
        );
        crate::git::checkout_head_to_working("test", &repo_path).await.unwrap();

        let working = root.join("working");
        fs::write(working.join("settings.json"), "user data").unwrap();
        assert!(working.join("templates/.git").is_file());
        let program = ProgramFiles::git(&repo_path, Some("v1.0.0")).unwrap();
        assert!(program.files.iter().any(|file| file == "templates/template.txt"));
        let new_body = root.join("mirror");
        fs::create_dir_all(&new_body).unwrap();
        migrate_users(&working, &new_body, &program, &ProgramFiles::new([]).unwrap(), || false).unwrap();
        assert_eq!(fs::read_to_string(new_body.join("settings.json")).unwrap(), "user data");
        assert!(!new_body.join("templates/.git").exists());
        assert!(!new_body.join("templates/template.txt").exists());

        drop(source);
        drop(module);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn user_collisions_preserve_the_old_files_without_partial_migration() {
        let root = std::env::temp_dir().join(format!(
            "pyappify-user-conflict-{:x}",
            rand::random::<u64>()
        ));
        let old = root.join("old");
        let new = root.join("new");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("a-user"), b"user").unwrap();
        fs::write(old.join("z-conflict"), b"important").unwrap();
        fs::write(new.join("z-conflict"), b"program").unwrap();
        let error = migrate_users(
            &old,
            &new,
            &ProgramFiles::new(Vec::new()).unwrap(),
            &ProgramFiles::new(vec!["z-conflict".into()]).unwrap(),
            || false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("z-conflict"));
        assert_eq!(fs::read(old.join("z-conflict")).unwrap(), b"important");
        assert_eq!(fs::read(new.join("z-conflict")).unwrap(), b"program");
        assert!(!new.join("a-user").exists());
        fs::remove_dir_all(root).unwrap();
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

pub fn migrate_users(
    old: &Path,
    new: &Path,
    old_program: &ProgramFiles,
    new_program: &ProgramFiles,
    cancelled: impl Fn() -> bool + Sync,
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
    let mut copies = Vec::new();
    for entry in walkdir::WalkDir::new(old)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let value = entry.path().strip_prefix(old).unwrap_or(Path::new(""));
            !value.components().next().is_some_and(|part| part.as_os_str().eq_ignore_ascii_case("_internal"))
                && !value.components().any(|part| part.as_os_str().eq_ignore_ascii_case(".git"))
        })
    {
        if cancelled() {
            return Err(crate::utils::error::Error::Cancelled.into());
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
        if old_keys.contains(&normalized) || normalized == ".pip_update_needed.tmp" {
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
        if collision
            || new_keys.contains(&normalized)
            || new_directories.contains(&normalized)
            || target.exists()
        {
            bail!("User file conflicts with the new installation: {name}; the old installation and user files have been kept");
        }
        copies.push((entry.into_path(), target));
    }
    // Check every conflict before copying any user file into the new body.
    for (source, target) in copies {
        if cancelled() {
            return Err(crate::utils::error::Error::Cancelled.into());
        }
        fs::create_dir_all(target.parent().unwrap())?;
        fs::copy(source, target)?;
    }
    Ok(())
}
