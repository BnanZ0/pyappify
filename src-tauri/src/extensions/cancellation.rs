//! Runtime task identity. Persistent recovery remains source-owned.
use crate::{emitter, utils::error::Error};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Notify;

static CURRENT_OPERATION: Mutex<Option<ActiveOperation>> = Mutex::new(None);
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    GitInstall,
    GitUpdate,
    GitConfigure,
    MirrorInstall,
    MirrorUpdate,
    MirrorConfigure,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Operation {
    pub id: String,
    pub sequence: u64,
    pub app_name: String,
    pub kind: OperationKind,
    pub status: OperationStatus,
    pub can_cancel: bool,
    pub target_version: Option<String>,
    pub previous_version: Option<String>,
    pub profile: Option<String>,
    pub error: Option<String>,
}
impl Operation {
    fn active(&self) -> bool {
        matches!(
            self.status,
            OperationStatus::Running | OperationStatus::Cancelling
        )
    }
}

#[derive(Default)]
pub struct Cancellation {
    requested: AtomicBool,
    shielded: AtomicBool,
    git_aborted: AtomicBool,
    notify: Notify,
}
impl Cancellation {
    pub fn check(&self) -> Result<(), Error> {
        if self.requested.load(Ordering::SeqCst) && !self.shielded.load(Ordering::SeqCst) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
    pub(crate) fn git_progress(&self) -> bool {
        if self.check().is_err() {
            self.git_aborted.store(true, Ordering::SeqCst);
            false
        } else {
            true
        }
    }
    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.check().is_err() {
                return;
            }
            notified.await;
        }
    }
    pub async fn run<F: Future>(&self, future: F) -> Result<F::Output, Error> {
        self.check()?;
        tokio::select! { biased;
            _ = self.cancelled() => Err(Error::Cancelled),
            result = future => Ok(result),
        }
    }
}

struct ActiveOperation {
    operation: Operation,
    cancellation: Arc<Cancellation>,
}

pub(crate) fn current_operation(app_name: &str) -> Option<Operation> {
    CURRENT_OPERATION
        .lock()
        .unwrap()
        .as_ref()
        .filter(|task| task.operation.app_name == app_name)
        .map(|task| task.operation.clone())
}
pub(crate) fn current_kind(app_name: &str) -> Option<OperationKind> {
    current_operation(app_name)
        .filter(Operation::active)
        .map(|task| task.kind)
}
pub(crate) fn current_id(app_name: &str) -> Option<String> {
    current_operation(app_name)
        .filter(Operation::active)
        .map(|task| task.id)
}

/// Snapshot the active foreground task. Background callers must capture their owner at dispatch.
pub(crate) fn current_log_context() -> Option<(String, String)> {
    CURRENT_OPERATION
        .lock()
        .unwrap()
        .as_ref()
        .filter(|task| task.operation.active())
        .map(|task| (task.operation.app_name.clone(), task.operation.id.clone()))
}

pub(crate) fn current_token(app_name: &str) -> Option<Arc<Cancellation>> {
    CURRENT_OPERATION
        .lock()
        .unwrap()
        .as_ref()
        .filter(|task| task.operation.app_name == app_name && task.operation.active())
        .map(|task| task.cancellation.clone())
}
pub(crate) fn app_operation_cancelled() -> bool {
    CURRENT_OPERATION
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|task| task.operation.active() && task.cancellation.check().is_err())
}
pub(crate) fn ensure_app_operation_not_cancelled() -> Result<(), Error> {
    if app_operation_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
pub(crate) async fn wait<F: Future>(app_name: &str, future: F) -> Result<F::Output, Error> {
    match current_token(app_name) {
        Some(token) => token.run(future).await,
        None => Ok(future.await),
    }
}

/// Transfer/indexer callbacks can abort with either User or Generic/Callback.
/// Classify cancellation only when this task actually aborted its callback.
pub(crate) fn git_result<T>(app_name: &str, result: anyhow::Result<T>) -> anyhow::Result<T> {
    if let Some(token) = current_token(app_name) {
        let aborted = token.git_aborted.swap(false, Ordering::SeqCst);
        if aborted
            && result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<git2::Error>())
                .is_some_and(|error| {
                    error.code() == git2::ErrorCode::User
                        || error.class() == git2::ErrorClass::Callback
                })
        {
            token.check()?;
        }
    }
    result
}
pub(crate) fn is_cancelled(error: &Error) -> bool {
    matches!(error, Error::Cancelled)
        || matches!(error, Error::Anyhow(error) if error.chain().any(|source| source.downcast_ref::<Error>().is_some_and(|error| matches!(error, Error::Cancelled))))
}

pub(crate) fn request_cancellation(app_name: &str, id: &str) -> bool {
    let snapshot = {
        let mut current = CURRENT_OPERATION.lock().unwrap();
        let Some(task) = current.as_mut().filter(|task| {
            task.operation.app_name == app_name
                && task.operation.id == id
                && task.operation.active()
                && task.operation.can_cancel
        }) else {
            return false;
        };
        if task.operation.status == OperationStatus::Cancelling {
            return true;
        }
        task.cancellation.requested.store(true, Ordering::SeqCst);
        task.cancellation.notify.notify_waiters();
        task.operation.status = OperationStatus::Cancelling;
        task.operation.sequence = SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
        task.operation.clone()
    };
    emitter::emit("app-operation", snapshot);
    true
}

/// Atomically close cancellation before writing a commit marker.
pub(crate) fn begin_commit() -> Result<(), Error> {
    let snapshot = {
        let mut current = CURRENT_OPERATION.lock().unwrap();
        let Some(task) = current.as_mut().filter(|task| task.operation.active()) else {
            return Ok(());
        };
        task.cancellation.check()?;
        task.operation.can_cancel = false;
        task.operation.sequence = SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
        task.operation.clone()
    };
    emitter::emit("app-operation", snapshot);
    Ok(())
}

/// Rollback ignores cancellation without erasing the original request/result.
pub(crate) struct RecoveryShield(Option<Arc<Cancellation>>);
pub(crate) fn shield_recovery(app_name: &str) -> RecoveryShield {
    let token = current_token(app_name);
    if let Some(token) = &token {
        token.shielded.store(true, Ordering::SeqCst);
    }
    RecoveryShield(token)
}
impl Drop for RecoveryShield {
    fn drop(&mut self) {
        if let Some(token) = &self.0 {
            token.shielded.store(false, Ordering::SeqCst);
        }
    }
}

pub(crate) struct AppOperationCancellationGuard {
    id: String,
    finished: bool,
}
impl AppOperationCancellationGuard {
    pub(crate) fn start(
        app_name: &str,
        kind: OperationKind,
        id: Option<String>,
        target_version: Option<String>,
        profile: Option<String>,
        previous_version: Option<String>,
    ) -> Self {
        let sequence = SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
        let operation = Operation {
            id: id.unwrap_or_else(|| format!("{:032x}-{sequence}", rand::random::<u128>())),
            sequence,
            app_name: app_name.into(),
            kind,
            status: OperationStatus::Running,
            can_cancel: true,
            target_version,
            previous_version,
            profile,
            error: None,
        };
        *CURRENT_OPERATION.lock().unwrap() = Some(ActiveOperation {
            operation: operation.clone(),
            cancellation: Arc::default(),
        });
        emitter::emit("app-operation", operation.clone());
        Self {
            id: operation.id,
            finished: false,
        }
    }
    pub(crate) fn finish(&mut self, result: &Result<(), Error>) {
        let snapshot = {
            let mut current = CURRENT_OPERATION.lock().unwrap();
            let Some(task) = current.as_mut().filter(|task| task.operation.id == self.id) else {
                return;
            };
            task.operation.status = match result {
                Ok(()) => OperationStatus::Succeeded,
                Err(error) if is_cancelled(error) => OperationStatus::Cancelled,
                Err(_) => OperationStatus::Failed,
            };
            task.operation.can_cancel = false;
            task.operation.error = result
                .as_ref()
                .err()
                .filter(|error| !is_cancelled(error))
                .map(ToString::to_string);
            task.operation.sequence = SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
            task.operation.clone()
        };
        self.finished = true;
        emitter::emit("app-operation", snapshot);
    }
}
impl Drop for AppOperationCancellationGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(&Err(Error::Msg("Operation ended without a result".into())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_is_scoped_to_the_task_and_waits_for_recovery_before_finishing() {
        let mut task = AppOperationCancellationGuard::start(
            "fixture",
            OperationKind::GitInstall,
            Some("first".into()),
            None,
            None,
            None,
        );
        let token = current_token("fixture").unwrap();
        assert!(!request_cancellation("fixture", "stale"));
        assert!(token.check().is_ok());
        let waiting = token.run(std::future::pending::<()>());
        tokio::pin!(waiting);
        assert!(matches!(
            futures_util::poll!(&mut waiting),
            std::task::Poll::Pending
        ));
        assert!(request_cancellation("fixture", "first"));
        let cancelling = current_operation("fixture").unwrap();
        assert!(request_cancellation("fixture", "first"));
        assert_eq!(current_operation("fixture").unwrap(), cancelling);
        assert!(matches!(waiting.await, Err(Error::Cancelled)));
        assert_eq!(
            current_operation("fixture").unwrap().status,
            OperationStatus::Cancelling
        );
        {
            let _recovery = shield_recovery("fixture");
            assert!(token.check().is_ok());
            assert_eq!(
                current_operation("fixture").unwrap().status,
                OperationStatus::Cancelling
            );
        }
        assert!(token.check().is_err());
        // libgit2's indexer abort is Generic/Callback, not necessarily User.
        let callback_error = || {
            anyhow::Error::new(git2::Error::new(
                git2::ErrorCode::GenericError,
                git2::ErrorClass::Callback,
                "indexer progress callback returned -1",
            ))
        };
        assert!(!is_cancelled(&Error::Anyhow(
            git_result::<()>("fixture", Err(callback_error())).unwrap_err()
        )));
        assert!(!token.git_progress());
        assert!(is_cancelled(&Error::Anyhow(
            git_result::<()>("fixture", Err(callback_error())).unwrap_err()
        )));
        assert!(!token.git_progress());
        let network_error = git2::Error::new(
            git2::ErrorCode::GenericError,
            git2::ErrorClass::Net,
            "Connection failed",
        );
        assert!(!is_cancelled(&Error::Anyhow(
            git_result::<()>("fixture", Err(network_error.into())).unwrap_err()
        )));
        assert!(matches!(begin_commit(), Err(Error::Cancelled)));
        task.finish(&Err(Error::Cancelled));
        assert_eq!(
            current_operation("fixture").unwrap().status,
            OperationStatus::Cancelled
        );
        assert!(current_token("fixture").is_none());

        let mut next = AppOperationCancellationGuard::start(
            "fixture",
            OperationKind::GitUpdate,
            Some("second".into()),
            None,
            None,
            None,
        );
        assert!(!request_cancellation("fixture", "first"));
        begin_commit().unwrap();
        assert!(!request_cancellation("fixture", "second"));
        next.finish(&Ok(()));
        assert_eq!(
            current_operation("fixture").unwrap().status,
            OperationStatus::Succeeded
        );

        let mut failing = AppOperationCancellationGuard::start(
            "fixture",
            OperationKind::MirrorInstall,
            Some("third".into()),
            None,
            None,
            None,
        );
        request_cancellation("fixture", "third");
        failing.finish(&Err(Error::Msg("Recovery failed".into())));
        assert_eq!(
            current_operation("fixture").unwrap().status,
            OperationStatus::Failed
        );
        assert!(is_cancelled(&Error::Anyhow(
            anyhow::Error::new(Error::Cancelled).context("download failed")
        )));
        assert!(!is_cancelled(&Error::Msg("download failed".into())));
    }
}
