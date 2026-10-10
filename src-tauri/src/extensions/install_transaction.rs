//! Durable directory transactions. Callers own package validation and installation completeness.
use super::file_operations::{
    check_cancelled, is_reparse, key, read_json, relative, safe_join, within,
};
use super::restart_manager::RmFileScope;
use crate::extensions::file_operations::{self as file_ops, DeletePolicy};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
pub(crate) const TASK_DIR: &str = ".pyappify-update";
pub fn replacement_pending(root: &Path) -> bool {
    let task = root.join(TASK_DIR);
    task.join("journal.json").is_file() && !task.join("committed").is_file()
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Action {
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
    /// A directory created by the Git installer, without creating it during backup.
    OwnedTree {
        path: String,
        backup: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Journal {
    pub(crate) app_name: String,
    pub(crate) actions: Vec<Action>,
    pub(crate) rollback_config: String,
    // Older journals used all-file registration for every operation. Preserve
    // that behavior when recovering them; new transactions write an explicit value.
    #[serde(default = "legacy_rm_all_files")]
    rm_all_files: bool,
}

fn legacy_rm_all_files() -> bool {
    true
}

impl Journal {
    fn rm_scope(&self) -> RmFileScope {
        if self.rm_all_files {
            RmFileScope::AllFiles
        } else {
            RmFileScope::NativeImages
        }
    }
}

/// Both distribution routes commit through the same durable file journal.
pub struct InstallTransaction {
    pub(crate) root: PathBuf,
    pub(crate) app_name: String,
    pub(crate) actions: Vec<Action>,
    displaced: BTreeSet<String>,
    created_dirs: BTreeSet<String>,
    applied: usize,
    rm_all_files: bool,
}

impl InstallTransaction {
    pub(crate) fn new(root: &Path, app_name: &str) -> Result<Self> {
        if relative(app_name)?.components().count() != 1 {
            bail!("Invalid installation identity");
        }
        Ok(Self {
            root: root.into(),
            app_name: app_name.into(),
            actions: Vec::new(),
            displaced: BTreeSet::new(),
            created_dirs: BTreeSet::new(),
            applied: 0,
            rm_all_files: false,
        })
    }

    pub(crate) fn base(&self) -> String {
        format!("data/apps/{}", self.app_name)
    }

    pub(crate) fn preserve_all_file_occupancy(&mut self) {
        self.rm_all_files = true;
    }

    /// Apply a previously planned directory preparation under a durable journal.
    pub(crate) fn apply_preparation(
        &mut self,
        rollback_config: &str,
        cancelled: impl Fn() -> bool + Sync,
    ) -> Result<()> {
        let journal = self.journal(rollback_config);
        validate_journal(&journal)?;
        durable_write(
            &self.root.join(TASK_DIR).join("journal.json"),
            &serde_json::to_vec(&journal)?,
        )?;
        for action in self.actions.iter().skip(self.applied) {
            check_cancelled(&cancelled)?;
            apply_action(&self.root, action, &cancelled, journal.rm_scope())?;
        }
        self.applied = self.actions.len();
        Ok(())
    }
    fn journal(&self, rollback_config: &str) -> Journal {
        Journal {
            app_name: self.app_name.clone(),
            actions: self.actions.clone(),
            rollback_config: rollback_config.into(),
            rm_all_files: self.rm_all_files,
        }
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

    pub(crate) fn mkdir(&mut self, value: &str) -> Result<()> {
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

    pub(crate) fn parents(&mut self, value: &str) -> Result<()> {
        if within(value, TASK_DIR) {
            return Ok(());
        }
        let parent = value.rsplit_once('/').map(|(parent, _)| parent);
        if let Some(parent) = parent {
            self.mkdir(parent)?;
        }
        Ok(())
    }

    pub(crate) fn backup(&mut self, value: &str) -> Result<Option<String>> {
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

    pub(crate) fn replace(&mut self, value: &str) -> Result<()> {
        self.backup(value)?;
        self.parents(value)?;
        self.actions.push(Action::Move {
            from: format!("{TASK_DIR}/staged/{value}"),
            to: value.into(),
        });
        Ok(())
    }
}

impl InstallTransaction {
    pub fn apply(
        mut self,
        config: &[u8],
        rollback_config: &str,
        cancelled: impl Fn() -> bool + Sync,
    ) -> Result<()> {
        let base = self.base();
        let name = format!("{base}/app.json");
        let path = self.root.join(TASK_DIR).join("staged").join(&name);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, config)?;
        self.replace(&name)?;
        let journal = self.journal(rollback_config);
        let task = self.root.join(TASK_DIR);
        validate_journal(&journal)?;
        durable_write(&task.join("journal.json"), &serde_json::to_vec(&journal)?)?;
        let result = (|| -> Result<()> {
            for action in journal.actions.iter().skip(self.applied) {
                check_cancelled(&cancelled)?;
                apply_action(&self.root, action, &cancelled, journal.rm_scope())?;
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
            file_ops::remove_tree(
                &task,
                DeletePolicy::Disposable,
                "Mirror rollback staging cleanup",
                &|| false,
            )
            .context("Files were restored, but staging cleanup is incomplete")?;
            return Err(error);
        }
        // Retire only a committed transaction; failed recovery keeps its backups.
        Ok(())
    }
}

fn apply_action(
    root: &Path,
    action: &Action,
    cancelled: &(dyn Fn() -> bool + Sync),
    rm_scope: RmFileScope,
) -> Result<()> {
    match action {
        Action::Move { from, to } => {
            let source = safe_join(root, from)?;
            let destination = safe_join(root, to)?;
            fs::create_dir_all(destination.parent().unwrap())?;
            if destination.exists() {
                bail!("Installation destination unexpectedly exists: {to}");
            }
            file_ops::move_path_with_rm_scope(
                &source,
                &destination,
                "Mirror transaction apply",
                cancelled,
                rm_scope,
            )?;
        }
        Action::Mkdir { path } => {
            fs::create_dir_all(safe_join(root, path)?)?;
        }
        Action::OwnedTree { .. } => {} // The Git installer creates the new tree.
        Action::RemoveEmptyDir { path } => file_ops::remove_empty_dir_with_rm_scope(
            &safe_join(root, path)?,
            "Mirror transaction empty directory",
            cancelled,
            rm_scope,
        )?,
    }
    Ok(())
}

use crate::extensions::file_operations::atomic_write as durable_write;

fn validate_journal(journal: &Journal) -> Result<()> {
    let app_name = journal.app_name.as_str();
    if relative(app_name)?.components().count() != 1 {
        bail!("Invalid installation recovery identity");
    }
    let base = format!("data/apps/{app_name}");
    for action in &journal.actions {
        let paths = match action {
            Action::Move { from, to } => vec![from, to],
            Action::Mkdir { path } | Action::RemoveEmptyDir { path } => vec![path],
            Action::OwnedTree { path, backup } => {
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
        };
        for path in paths {
            relative(path)?;
            if !within(path, TASK_DIR)
                && !within(path, &format!("{base}/working"))
                && ![
                    "data".into(),
                    "data/apps".into(),
                    base.clone(),
                    format!("{base}/working"),
                    format!("{base}/python"),
                    format!("{base}/repo"),
                    format!("{base}/app.json"),
                ]
                .contains(path)
            {
                bail!("Invalid ZIP recovery journal path");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rm_scope_survives_journaling_and_legacy_recovery() {
        let mut transaction =
            InstallTransaction::new(Path::new("unused-fixture"), "example").unwrap();
        let native = transaction.journal("{}");
        assert!(!native.rm_all_files);
        let persisted: Journal =
            serde_json::from_slice(&serde_json::to_vec(&native).unwrap()).unwrap();
        assert!(persisted.rm_scope() == RmFileScope::NativeImages);
        transaction.preserve_all_file_occupancy();
        let delta = transaction.journal("{}");
        let persisted: Journal =
            serde_json::from_slice(&serde_json::to_vec(&delta).unwrap()).unwrap();
        assert!(persisted.rm_scope() == RmFileScope::AllFiles);
        let legacy: Journal = serde_json::from_value(serde_json::json!({
            "app_name": "example", "actions": [], "rollback_config": "{}"
        }))
        .unwrap();
        assert!(legacy.rm_scope() == RmFileScope::AllFiles);
    }

    #[test]
    fn cancelled_commit_restores_the_body_and_config_through_shared_file_operations() {
        let root =
            std::env::temp_dir().join(format!("pyappify-transaction-{:x}", rand::random::<u64>()));
        let working = root.join("data/apps/example/working");
        let config = root.join("data/apps/example/app.json");
        fs::create_dir_all(&working).unwrap();
        fs::write(working.join("body"), b"old body").unwrap();
        fs::write(&config, b"{\"version\":\"old\"}").unwrap();
        let mut transaction = InstallTransaction::new(&root, "example").unwrap();
        transaction.backup("data/apps/example/working").unwrap();
        fs::create_dir_all(root.join(TASK_DIR).join("staged")).unwrap();
        // All fixture operations succeed directly or cancel; native RM is not called.
        transaction
            .apply_preparation("{\"version\":\"old\"}", || false)
            .unwrap();
        let staged = root.join(TASK_DIR).join("staged/data/apps/example/working");
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("body"), b"new body").unwrap();
        transaction.replace("data/apps/example/working").unwrap();
        let result = transaction.apply(b"{\"version\":\"new\"}", "{\"version\":\"old\"}", || {
            fs::read(working.join("body")).is_ok_and(|bytes| bytes == b"new body")
        });
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(fs::read(working.join("body")).unwrap(), b"old body");
        assert_eq!(fs::read(&config).unwrap(), b"{\"version\":\"old\"}");
        assert!(!root.join(TASK_DIR).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_live_file_and_backup_keep_the_recovery_journal_and_current_config() {
        let root =
            std::env::temp_dir().join(format!("pyappify-lost-backup-{:x}", rand::random::<u64>()));
        let working = root.join("data/apps/example/working");
        fs::create_dir_all(&working).unwrap();
        fs::write(working.join("body"), b"old body").unwrap();
        let config = root.join("data/apps/example/app.json");
        fs::write(&config, b"{\"version\":\"current\"}").unwrap();
        let mut transaction = InstallTransaction::new(&root, "example").unwrap();
        let backup = transaction
            .backup("data/apps/example/working/body")
            .unwrap()
            .unwrap();
        fs::create_dir_all(root.join(TASK_DIR).join("staged")).unwrap();
        transaction
            .apply_preparation("{\"version\":\"old\"}", || false)
            .unwrap();
        // Simulate an external removal of the only recoverable copy after the move.
        fs::remove_file(root.join(backup)).unwrap();
        let recovered = recover(&root);
        assert!(
            recovered.is_err(),
            "recovery reported success with neither file copy available"
        );
        assert!(replacement_pending(&root));
        assert!(root.join(TASK_DIR).join("journal.json").is_file());
        assert_eq!(fs::read(config).unwrap(), b"{\"version\":\"current\"}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_restores_the_original_installation_after_every_interrupted_action() {
        // Enumerate every durable action boundary, including the config move.
        for applied in 0.. {
            let root = std::env::temp_dir().join(format!(
                "pyappify-crash-boundary-{:x}",
                rand::random::<u64>()
            ));
            let working = root.join("data/apps/example/working");
            let task = root.join(TASK_DIR);
            fs::create_dir_all(working.join("retired")).unwrap();
            fs::write(working.join("body"), b"old body").unwrap();
            fs::write(working.join("retired/resource"), b"old resource").unwrap();
            fs::write(working.join("user"), b"user data").unwrap();
            let config = root.join("data/apps/example/app.json");
            fs::write(&config, b"{\"version\":\"old\"}").unwrap();
            let mut transaction = InstallTransaction::new(&root, "example").unwrap();
            for (path, bytes) in [
                ("data/apps/example/working/body", b"new body".as_slice()),
                (
                    "data/apps/example/working/new/sub/resource",
                    b"new resource".as_slice(),
                ),
                (
                    "data/apps/example/app.json",
                    b"{\"version\":\"new\"}".as_slice(),
                ),
            ] {
                let staged = task.join("staged").join(path);
                fs::create_dir_all(staged.parent().unwrap()).unwrap();
                fs::write(staged, bytes).unwrap();
                transaction.replace(path).unwrap();
            }
            transaction
                .backup("data/apps/example/working/retired/resource")
                .unwrap();
            transaction.actions.push(Action::RemoveEmptyDir {
                path: "data/apps/example/working/retired".into(),
            });
            let journal = transaction.journal("{\"version\":\"old\"}");
            let actions = journal.actions.len();
            durable_write(
                &task.join("journal.json"),
                &serde_json::to_vec(&journal).unwrap(),
            )
            .unwrap();
            for action in journal.actions.iter().take(applied) {
                apply_action(&root, action, &|| false, journal.rm_scope()).unwrap();
            }
            assert!(
                recover(&root).unwrap(),
                "recovery was skipped after action {applied}"
            );
            assert_eq!(
                fs::read(working.join("body")).unwrap(),
                b"old body",
                "body differs after action {applied}"
            );
            assert_eq!(
                fs::read(working.join("retired/resource")).unwrap(),
                b"old resource",
                "resource differs after action {applied}"
            );
            assert_eq!(
                fs::read(working.join("user")).unwrap(),
                b"user data",
                "user data differs after action {applied}"
            );
            assert_eq!(
                fs::read(&config).unwrap(),
                b"{\"version\":\"old\"}",
                "config differs after action {applied}"
            );
            assert!(
                !working.join("new").exists(),
                "new directories remained after action {applied}"
            );
            assert!(!task.exists(), "staging remained after action {applied}");
            assert!(
                !recover(&root).unwrap(),
                "second recovery was not idempotent after action {applied}"
            );
            fs::remove_dir_all(root).unwrap();
            if applied == actions {
                break;
            }
        }
    }

    #[test]
    fn recovery_journal_allows_working_yaml_but_rejects_a_root_yaml_move() {
        let journal = |from: &str| Journal {
            app_name: "example".into(),
            actions: vec![Action::Move {
                from: from.into(),
                to: format!("{TASK_DIR}/backup/0"),
            }],
            rollback_config: "{}".into(),
            rm_all_files: false,
        };
        assert!(validate_journal(&journal("pyappify.yml")).is_err());
        assert!(validate_journal(&journal("data/apps/example/working")).is_ok());
        assert!(validate_journal(&journal("pyappify-release.json")).is_err());
        assert!(validate_journal(&journal("data/apps/example/app.json")).is_ok());
    }

    #[test]
    fn retired_cleanup_leaves_the_next_transaction_and_live_body() {
        let root =
            std::env::temp_dir().join(format!("pyappify-cleanup-{:x}", rand::random::<u64>()));
        let task = root.join(TASK_DIR);
        let live = root.join("data/apps/example/working/body");
        fs::create_dir_all(live.parent().unwrap()).unwrap();
        fs::write(&live, b"installed").unwrap();
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("committed"), b"1").unwrap();
        fs::write(task.join("backup"), b"old").unwrap();
        let retired = retire(&root).unwrap();
        assert_eq!(fs::read(retired.join("backup")).unwrap(), b"old");
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("committed"), b"1").unwrap();
        fs::write(task.join("backup"), b"next").unwrap();
        // A partial deletion may already have removed the old commit marker.
        fs::remove_file(retired.join("committed")).unwrap();
        cleanup(&root).unwrap();
        assert!(!retired.exists());
        assert_eq!(fs::read(task.join("backup")).unwrap(), b"next");
        assert_eq!(fs::read(live).unwrap(), b"installed");
        fs::remove_dir_all(root).unwrap();
    }
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
                    file_ops::move_path_with_rm_scope(
                        &destination,
                        &source,
                        "Mirror transaction rollback",
                        &|| false,
                        journal.rm_scope(),
                    )
                    .with_context(|| format!("Unable to restore {from}"))?;
                } else if source.exists()
                    && destination.exists()
                    && !within(from, TASK_DIR)
                    && within(to, TASK_DIR)
                {
                    bail!("Unable to restore {from}: the live path reappeared; the recovery journal and backup have been kept");
                } else if !source.exists()
                    && !destination.exists()
                    && !within(from, TASK_DIR)
                    && within(to, TASK_DIR)
                {
                    bail!("Unable to restore {from}: both the live path and its backup are missing; the recovery journal has been kept");
                }
            }
            Action::Mkdir { path } => {
                let _ = file_ops::remove_empty_dir_with_rm_scope(
                    &safe_join(root, path)?,
                    "Mirror rollback empty directory",
                    &|| false,
                    journal.rm_scope(),
                );
            }
            Action::RemoveEmptyDir { path } => {
                fs::create_dir_all(safe_join(root, path)?)?;
            }
            Action::OwnedTree { path, backup } => {
                // A failed backup move must never cause removal of the old tree.
                if backup
                    .as_ref()
                    .map(|p| safe_join(root, p).map(|p| p.exists()))
                    .transpose()?
                    .unwrap_or(true)
                {
                    let path = safe_join(root, path)?;
                    if path.exists() {
                        file_ops::remove_tree(
                            &path,
                            DeletePolicy::Disposable,
                            "Mirror rollback owned tree",
                            &|| false,
                        )?;
                    }
                }
            }
        }
    }
    let app_name = &journal.app_name;
    let path = safe_join(root, &format!("data/apps/{app_name}/app.json"))?;
    fs::create_dir_all(path.parent().unwrap())?;
    durable_write(&path, journal.rollback_config.as_bytes())?;
    Ok(())
}

/// Detach this committed transaction before releasing the installation lock.
pub(crate) fn retire(root: &Path) -> Result<PathBuf> {
    let task = safe_join(root, TASK_DIR)?;
    let retired = root.join(format!("{TASK_DIR}-completed-{:x}", rand::random::<u64>()));
    file_ops::move_path(&task, &retired, "Mirror retire committed backup", &|| false)?;
    Ok(retired)
}

/// Run before reading app.json, so an interrupted apply cannot publish a new
/// version with old files. A source switch journals its preparation before
/// download; staging without a journal has not displaced any installed files.
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
        file_ops::remove_tree(
            &task,
            DeletePolicy::Disposable,
            "Mirror recovered staging cleanup",
            &|| false,
        )?;
        return Ok(true);
    }
    file_ops::remove_tree(
        &task,
        DeletePolicy::Disposable,
        "Mirror uncommitted staging cleanup",
        &|| false,
    )?;
    Ok(false)
}

/// Startup cleanup never retires or inspects the active transaction.
pub fn cleanup(root: &Path) -> Result<()> {
    let mut errors = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{TASK_DIR}-completed-"))
        {
            if !is_reparse(&fs::symlink_metadata(entry.path())?) {
                if let Err(error) = file_ops::remove_tree_with_log(
                    &entry.path(),
                    DeletePolicy::Disposable,
                    "Mirror committed backup cleanup",
                    &|| false,
                    None,
                ) {
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
