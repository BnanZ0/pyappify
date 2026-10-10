import type {App, MessagePayload, Operation, Task} from '../../types';
import {getVersionActionType} from '../app/versions';

export const isTaskActive = (task?: Operation | null) => !!task && ['pending', 'running', 'cancelling'].includes(task.status);
export const isConfiguration = (task: Operation) => task.kind.endsWith('_configure');
export const isUpdate = (task: Operation) => task.kind.endsWith('_update');

// Persistent failure is a display projection, never a running/cancellable task.
export function taskForApp(task: Task | undefined, app: App | null, logs: MessagePayload[] = []): Task | undefined {
    if (task || !app) return task;
    const sourceFailed = app.source_operation_state === 'failed';
    if (!sourceFailed && app.update_state !== 'failed') return undefined;
    const error = (sourceFailed ? app.source_operation_error : app.update_error) ?? '';
    const target = sourceFailed ? app.source_operation_target : app.update_target_version;
    return {id: `failure:${app.name}`, sequence: 0, app_name: app.name,
        kind: sourceFailed ? app.source_operation_kind ?? 'mirror_install' : 'git_update', status: 'failed', can_cancel: false,
        target_version: target ?? null, profile: app.current_profile, error,
        action: getVersionActionType(target ?? '', app.current_version, 'Upgrade'),
        logs: error && !logs.some(log => log.message.includes(error)) ? appendLog(logs, {app_name: app.name, message: error, error: true}) : logs};
}

export function acceptOperation(previous: Task | undefined, operation: Operation, app: App | null): Task {
    if (previous && (operation.sequence < previous.sequence ||
        (operation.id !== previous.id && previous.status === 'pending'))) return previous;
    if (previous?.id === operation.id) {
        if (!isTaskActive(previous) && isTaskActive(operation)) return previous;
        if (previous.status === 'cancelling' && operation.status === 'running' && operation.sequence <= previous.sequence) return previous;
        return {...previous, ...operation};
    }
    return {...operation, logs: [], action: getVersionActionType(operation.target_version ?? '', operation.previous_version ?? app?.current_version ?? null, 'Upgrade')};
}

export function appendLog(logs: MessagePayload[], log: MessagePayload): MessagePayload[] {
    const next = log.update && logs.length && logs[logs.length - 1].update
        ? [...logs.slice(0, -1), log] : [...logs, log];
    return next.slice(-500);
}

export function acceptTaskLog(task: Task | undefined, log: MessagePayload): Task | undefined {
    return task && log.operation_id === task.id ? {...task, logs: appendLog(task.logs, log)} : task;
}
