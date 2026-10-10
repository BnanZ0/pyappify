// src/command.rs
use crate::utils::error::Error;
use crate::{emit_error, emit_info, ensure_some, err};
use std::ffi::OsStr;
use std::process::{ExitStatus, Stdio};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tracing::{debug, error, info};
use windows_sys::Win32::UI::Shell::IsUserAnAdmin;

pub async fn run_command_and_stream_output(
    command: Command,
    app_name: &str,
    command_description: &str,
) -> Result<ExitStatus, Error> {
    run_command_inner(command, app_name, command_description, None).await
}

pub(crate) async fn run_command_cancellable(
    command: Command,
    app_name: &str,
    description: &str,
    token: std::sync::Arc<crate::extensions::cancellation::Cancellation>,
) -> Result<ExitStatus, Error> {
    run_command_inner(command, app_name, description, Some(token)).await
}

async fn run_command_inner(
    mut command: Command,
    app_name: &str,
    command_description: &str,
    token: Option<std::sync::Arc<crate::extensions::cancellation::Cancellation>>,
) -> Result<ExitStatus, Error> {
    emit_info!(
        app_name,
        "executing command: '{}'. Full details: {:?}",
        command_description,
        command
    );

    if let Some(token) = &token {
        token.check()?;
    }
    let job = token
        .as_ref()
        .map(|_| crate::extensions::process_job::ProcessJob::new())
        .transpose()?;
    command.creation_flags(if job.is_some() {
        0x08000000 | 0x00000004
    } else {
        0x08000000
    });
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|e| {
        let msg = format!("Failed to spawn command ({}): {}", command_description, e);
        error!(error = %e, command = %command_description, %msg);
        err!(msg)
    })?;

    if let Some(job) = &job {
        if let Err(error) = job.attach_and_resume(&child) {
            // Assignment/resume failure must not leave a suspended writer alive.
            let stop = child.kill().await;
            let _ = child.wait().await;
            stop?;
            return Err(error.into());
        }
    }
    let operation_id = token
        .as_ref()
        .and_then(|_| crate::extensions::cancellation::current_id(app_name));
    let stdout_operation = operation_id.clone();
    let stderr_operation = operation_id;
    let child_pid = child
        .id()
        .map(|id| id.to_string())
        .unwrap_or_else(|| "N/A".to_string());
    info!(pid = %child_pid, cmd_desc = %command_description, "Command spawned");

    let stdout = ensure_some!(
        child.stdout.take(),
        "Could not capture stdout from command ({})",
        command_description
    )
    .map_err(|e| {
        emit_error!(app_name, "{}", e.to_string());
        err!(e.to_string())
    })?;

    let stderr = ensure_some!(
        child.stderr.take(),
        "Could not capture stderr from command ({})",
        command_description
    )
    .map_err(|e| {
        emit_error!(app_name, "{}", e.to_string());
        err!(e.to_string())
    })?;

    let mut stdout_buf_reader = tokio::io::BufReader::new(stdout);
    let mut stderr_buf_reader = tokio::io::BufReader::new(stderr);

    let app_name_for_stdout = app_name.to_string();
    let stdout_task = tokio::spawn(async move {
        let mut buffer = String::new();
        loop {
            match stdout_buf_reader.read_line(&mut buffer).await {
                Ok(0) => break,
                Ok(_) => {
                    crate::emitter::emit_log_for_operation(
                        app_name_for_stdout.clone(),
                        buffer.as_str(),
                        false,
                        false,
                        stdout_operation.clone(),
                    );
                    buffer.clear();
                }
                Err(e) => {
                    emit_error!(app_name_for_stdout, "Error reading stdout line: {}", e);
                    break;
                }
            }
        }
    });

    let app_name_for_stderr = app_name.to_string();
    let stderr_task = tokio::spawn(async move {
        let mut buffer = String::new();
        loop {
            match stderr_buf_reader.read_line(&mut buffer).await {
                Ok(0) => break,
                Ok(_) => {
                    let err_string = buffer.to_string();
                    buffer.clear();
                    if !err_string.trim().is_empty()
                        && !err_string.contains("A new release of pip is available")
                        && !err_string.contains("[notice] To update, run")
                    {
                        crate::emitter::emit_log_for_operation(
                            app_name_for_stderr.clone(),
                            &err_string,
                            false,
                            true,
                            stderr_operation.clone(),
                        );
                    } else {
                        debug!("not emitting black listed stderr {}", err_string);
                    }
                }
                Err(e) => {
                    emit_error!(app_name_for_stderr, "Error reading stderr line: {}", e);
                    break;
                }
            }
        }
    });

    // Keep cancellation active until descendants have exited, even after pip exits.
    // Complete the process wait before draining both streams and returning a result.
    let result = async {
        let Some(token) = &token else {
            return child.wait().await.map_err(Error::from);
        };
        let job = job.as_ref().expect("cancellable command has a job");
        let mut cancelled = false;
        let mut termination_error = None;
        let status = tokio::select! { biased;
            _ = token.cancelled() => {
                cancelled = true;
                termination_error = job.terminate().err();
                if termination_error.is_some() { let _ = child.kill().await; }
                child.wait().await
            }
            status = child.wait() => status,
        };
        let failed_before_cancel =
            !cancelled && status.as_ref().is_ok_and(|status| !status.success());
        if cancelled {
            job.wait_empty().await?;
        } else {
            tokio::select! { biased;
                _ = token.cancelled() => {
                    cancelled = true;
                    termination_error = job.terminate().err();
                    job.wait_empty().await?;
                }
                result = job.wait_empty() => result?,
            }
        }
        let status = status?;
        if let Some(error) = termination_error {
            return Err(Error::Io(error));
        }
        // An already observed command failure remains a failure during cancellation.
        if !cancelled || failed_before_cancel {
            return Ok(status);
        }
        Err(Error::Cancelled)
    }
    .await;

    let (stdout_result, stderr_result) = tokio::join!(stdout_task, stderr_task);
    if let Some(e) = stdout_result.err().or_else(|| stderr_result.err()) {
        error!(error = %e, cmd_desc = %command_description, "Log reading task encountered an error. This does not necessarily mean the command itself failed.");
    }

    let status = result?;
    if !status.success() {
        return Err(err!("Command failed ({}): {}", command_description, status));
    }

    Ok(status)
}

pub fn command_to_string(command: &std::process::Command) -> String {
    let program_path = command.get_program();
    let arguments: Vec<&str> = command.get_args().filter_map(|arg| arg.to_str()).collect();
    let mut command_string = String::new();
    if let Some(path) = program_path.to_str() {
        command_string.push_str(path);
    } else {
        command_string.push_str("<non-UTF8 program path>");
    }
    for arg in arguments {
        command_string.push(' ');
        if arg.contains(' ') || arg.contains('"') {
            command_string.push('"');
            command_string.push_str(arg.replace('"', "\"\"").as_str());
            command_string.push('"');
        } else {
            command_string.push_str(arg);
        }
    }
    command_string
}

#[cfg(windows)]
pub fn is_admin() -> bool {
    unsafe { IsUserAnAdmin().is_positive() }
}

pub fn new_cmd<S: AsRef<OsStr>>(executable: S) -> Command {
    let mut command = Command::new(executable);
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000);
    }
    command
}

#[cfg(not(windows))]
pub async fn is_admin() -> bool {
    if let Ok(output) = Command::new("id").arg("-u").output().await {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim() == "0";
        }
    }
    false
}
