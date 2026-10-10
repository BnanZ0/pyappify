import {mirrorPhaseLabel} from '../mirror/progress';
import type {App, MessagePayload, MirrorProgress, Operation, StatusState, Task, VersionProposal} from '../../types';
import type {TFunction} from 'i18next';
import {acceptOperation, acceptTaskLog, appendLog, isTaskActive, isUpdate, taskForApp} from './model';
import {compareVersions, getVersionActionType, isReleaseVersion} from '../app/versions';
import {installationKind, installedSource, sourceNeedsInstallation} from '../mirror/operationUi';

export interface Transport {
    invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
    listen<T>(name: string, handler: (event: {payload: T}) => void): Promise<() => void>;
}
type Request = 'settings' | 'start' | 'stop' | 'delete' | 'refresh' | 'preferences' | 'defender';
export interface LauncherState {
    app: App | null;
    task?: Task;
    applicationLogs: MessagePayload[];
    applicationOutcome?: 'succeeded' | 'failed';
    request: Request | null;
    status: StatusState;
    proposal: VersionProposal | null;
    defenderHidden: boolean;
}
const errorText = (error: unknown) => typeof error === 'object' && error !== null && 'message' in error ? String(error.message) : String(error);
const cancelledError = (error: unknown) => typeof error === 'object' && error !== null && 'kind' in error && error.kind === 'cancelled';

export function createLauncherController(transport: Transport, translate: TFunction) {
    let t = translate;
    let state: LauncherState = {app: null, applicationLogs: [], request: null, status: {loading: true}, proposal: null, defenderHidden: false};
    const subscribers = new Set<() => void>();
    let notifiedTask: string | undefined;
    let manualTask: string | undefined;
    let proposalDismissed = false;
    let disposed = false;
    let connectionGeneration = 0;
    let requestGeneration = 0;
    const publish = (next: LauncherState) => { state = next; subscribers.forEach(listener => listener()); };
    const patch = (change: Partial<LauncherState>) => publish({...state, ...change});
    const updateStatus = (change: Partial<StatusState>) => patch({status: {...state.status, ...change}});
    const clearMessages = () => updateStatus({error: null, info: null});
    const storedLogs = (name: string): MessagePayload[] => {
        try {return JSON.parse(localStorage.getItem('pyappifyConsoleLogs') ?? '{}')[name] ?? [];}
        catch {return [];}
    };
    const persistLogs = (name: string, logs: MessagePayload[]) => {
        try {
            const stored = JSON.parse(localStorage.getItem('pyappifyConsoleLogs') ?? '{}');
            const otherStream = storedLogs(name).filter(log => !!log.operation_id !== !!logs[0]?.operation_id);
            localStorage.setItem('pyappifyConsoleLogs', JSON.stringify({...stored, [name]: [...otherStream, ...logs]}));
        } catch { /* Console history does not control task state. */ }
    };
    const notify = (task: Task) => {
        if (!isUpdate(task) || !task.target_version || notifiedTask === task.id || manualTask !== task.id) return;
        notifiedTask = task.id;
        if (task.status === 'cancelled') return;
        const title = `${t(`${task.action} ${task.status === 'succeeded' ? 'success' : 'failed'}`)}: ${task.app_name}`;
        void transport.invoke('send_notification_cmd', {title, body: task.notes ? `${task.target_version}\n${task.notes}` : task.target_version}).catch(console.warn);
    };
    const accept = (operation: Operation) => {
        const previous = state.task;
        let task = acceptOperation(previous, operation, state.app);
        if (task === previous) return;
        if (previous?.id !== task.id) task = {...task, logs: storedLogs(task.app_name).filter(log => log.operation_id === task.id)};
        if (task.status === 'failed' && task.error && !task.logs.some(log => log.error && log.message.includes(task.error!))) {
            task = {...task, logs: appendLog(task.logs, {app_name: task.app_name, operation_id: task.id, message: task.error, error: true})};
            persistLogs(task.app_name, task.logs);
        }
        patch({task});
        if (previous?.id === task.id && isTaskActive(previous) && !isTaskActive(task)) {
            notify(task);
            if (task.status === 'succeeded') { proposalDismissed = true; patch({proposal: isUpdate(task) && task.target_version ? {version: task.target_version, actionType: task.action} : null}); }
        }
    };
    const receiveApp = (app: App) => {
        // Application snapshots and task events have separate ordering domains.
        if (state.app && app.revision < state.app.revision) return;
        const previous = state.app;
        let proposal = state.proposal;
        if (previous?.update_source !== app.update_source) { proposal = null; proposalDismissed = false; }
        if (!proposal && !proposalDismissed && app.installed && !sourceNeedsInstallation(app) && !app.running && !isTaskActive(state.task)) {
            if (app.update_state === 'failed' && app.update_target_version) {
                proposal = {version: app.update_target_version, actionType: getVersionActionType(app.update_target_version, app.current_version, 'Upgrade')};
            } else {
                const latest = app.available_versions.filter(isReleaseVersion).sort((a, b) => compareVersions(b, a))[0];
                if (latest && app.current_version && compareVersions(latest, app.current_version) > 0) proposal = {version: latest, actionType: 'Upgrade'};
            }
        }
        let applicationLogs = state.applicationLogs;
        if (!previous && !applicationLogs.length) {
            applicationLogs = storedLogs(app.name).filter(log => !log.operation_id);
        }
        patch({app, proposal, applicationLogs, request: app.running && state.request === 'start' ? null : state.request, status: {...state.status, loading: false}});
        if (app.operation) accept(app.operation);
        else if (!previous) {
            const history = storedLogs(app.name).filter(log => !!log.operation_id);
            const lastId = history[history.length - 1]?.operation_id;
            patch({task: taskForApp(undefined, app, history.filter(log => log.operation_id === lastId))});
        }
    };
    const addLog = (log: MessagePayload) => {
        if (log.operation_id) {
            const task = acceptTaskLog(state.task, log);
            if (task !== state.task) { patch({task}); persistLogs(log.app_name, task!.logs); }
        } else {
            const applicationLogs = appendLog(state.applicationLogs, log);
            patch({applicationLogs, ...(log.finished ? {applicationOutcome: log.error ? 'failed' as const : 'succeeded' as const} : {})}); persistLogs(log.app_name, applicationLogs);
        }
    };
    const receiveProgress = (progress: MirrorProgress & {app_name: string}) => {
        const task = state.task;
        if (!task || task.id !== progress.operation_id) return;
        const fraction = progress.total ? Math.min(1, progress.downloaded / progress.total) : 0;
        const phaseValues: Record<string, number> = {
            downloading: fraction * 70, downloaded: 70, extracting: 70 + fraction * 29, installing: 99,
        };
        const value = Math.max(task.progress?.value ?? 0, Math.round(phaseValues[progress.phase] ?? 0));
        patch({task: {...task, progress: {...progress, value}}});
        const bucket = (value: MirrorProgress) => value.phase === 'downloading' && value.total ? Math.floor(value.downloaded / value.total * 10) : null;
        if (task.progress?.phase === progress.phase && bucket(task.progress) === bucket(progress)) return;
        const detail = progress.phase === 'downloading' && progress.total ? ` ${(progress.downloaded / 1024 / 1024).toFixed(1)} MB / ${(progress.total / 1024 / 1024).toFixed(1)} MB` : '';
        addLog({app_name: task.app_name, operation_id: task.id, message: t(mirrorPhaseLabel(progress.phase)) + detail, update: progress.phase === 'downloading', error: progress.phase.endsWith('_failed')});
    };
    const runTask = async (kind: Operation['kind'], command: string, args: Record<string, unknown>, message: string, notes?: string) => {
        const app = state.app;
        if (!app || isTaskActive(state.task) || state.request || app.running) return;
        clearMessages();
        const id = crypto.randomUUID();
        const task: Task = {
            id, sequence: (state.task?.sequence ?? 0) + 1, app_name: app.name,
            kind, status: 'pending', can_cancel: false, target_version: typeof args.version === 'string' ? args.version : null,
            previous_version: app.current_version, profile: typeof args.profileName === 'string' ? args.profileName : null,
            error: null, notes, logs: [{app_name: app.name, operation_id: id, message}], action: getVersionActionType(String(args.version ?? ''), app.current_version, 'Upgrade'),
        };
        manualTask = task.id;
        patch({task, proposal: isUpdate(task) && task.target_version ? {version: task.target_version, actionType: task.action} : null});
        const finish = (status: Operation['status'], error: string | null) => {
            if (state.task?.id !== task.id || !isTaskActive(state.task)) return;
            accept({...state.task, status, can_cancel: false, error});
        };
        try {
            await transport.invoke(command, {...args, operationId: task.id});
            // The command returns only after cleanup/recovery and backend finish.
            finish('succeeded', null);
        } catch (error) {
            finish(cancelledError(error) ? 'cancelled' : 'failed', cancelledError(error) ? null : errorText(error));
        }
    };
    const ensureSourceReady = async (operation: 'install' | 'update' = 'install') => {
        const app = state.app;
        if (!app || app.update_source !== 'mirrorchyan' || (operation === 'install' && installationKind(app) === 'mirror_configure')) return true;
        try { if (await transport.invoke<boolean>('mirrorchyan_has_cdk')) return true; } catch { /* Reconfigure an unreadable saved CDK. */ }
        updateStatus({info: t('Configure a MirrorChyan CDK in Settings before installing or updating.'), error: null});
        return false;
    };
    const request = async (kind: Request, command: string, args: Record<string, unknown> | undefined, failure: string, success?: () => void) => {
        if (state.request || isTaskActive(state.task)) return;
        clearMessages();
        const generation = ++requestGeneration;
        patch({request: kind});
        try { await transport.invoke(command, args); if (generation === requestGeneration) success?.(); }
        catch (error) { if (generation === requestGeneration) {
            updateStatus({error: `${failure}: ${errorText(error)}`});
            if (kind === 'start') { patch({applicationOutcome: 'failed'}); addLog({app_name: state.app!.name, message: `${failure}: ${errorText(error)}`, error: true}); }
        } }
        finally { if (!disposed && generation === requestGeneration) patch({request: null}); }
    };
    return {
        getSnapshot: () => state,
        subscribe: (listener: () => void) => {subscribers.add(listener); return () => {subscribers.delete(listener);};},
        setTranslator: (translate: TFunction) => {t = translate;},
        updateStatus, clearMessages, ensureSourceReady,
        async connect(onChooseProfile: (app: App) => void) {
            disposed = false;
            const generation = ++connectionGeneration;
            const guard = <T,>(handler: (event: {payload: T}) => void) => (event: {payload: T}) => {if (generation === connectionGeneration && !disposed) handler(event);};
            const registrations = await Promise.allSettled([
                transport.listen<App>('app', guard(event => receiveApp(event.payload))),
                transport.listen<Operation>('app-operation', guard(event => accept(event.payload))),
                transport.listen<MessagePayload>('app-log', guard(event => addLog(event.payload))),
                transport.listen<MirrorProgress & {app_name: string}>('mirror-update-progress', guard(event => receiveProgress(event.payload))),
                transport.listen<App>('choose_app_profile', guard(event => onChooseProfile(event.payload))),
                transport.listen<string>('mirrorchyan-cdk-required', guard(() => updateStatus({info: t('Configure a MirrorChyan CDK in Settings to enable automatic updates.')}))),
            ]);
            const unlisteners = registrations.flatMap(result => result.status === 'fulfilled' ? [result.value] : []);
            const failure = registrations.find(result => result.status === 'rejected');
            if (failure?.status === 'rejected') {
                unlisteners.forEach(unlisten => unlisten());
                if (!disposed && generation === connectionGeneration) updateStatus({loading: false, error: `Failed to load app: ${errorText(failure.reason)}`});
                return () => {};
            }
            if (disposed || generation !== connectionGeneration) { unlisteners.forEach(unlisten => unlisten()); return () => {}; }
            void transport.invoke('show_main_window').catch(console.warn);
            // Register every listener before load_app can publish a task or snapshot.
            void transport.invoke<App>('load_app').then(app => {if (!disposed && generation === connectionGeneration && !state.app) receiveApp(app);}).catch(error => {if (generation === connectionGeneration) updateStatus({loading: false, error: `Failed to load app: ${errorText(error)}`});});
            return () => {unlisteners.forEach(unlisten => unlisten());};
        },
        disconnect() { disposed = true; connectionGeneration++; },
        selectVersion(version: string) {
            proposalDismissed = !version;
            patch({proposal: version ? {version, actionType: getVersionActionType(version, state.app?.current_version ?? null)} : null});
        },
        install(profileName: string) {
            const app = state.app;
            if (!app) return Promise.resolve();
            return runTask(installationKind(app), 'setup_app', {appName: app.name, profileName}, `Initiating install for '${app.name}' with profile '${profileName}'...`);
        },
        configure(profileName: string) {
            const app = state.app;
            if (!app) return Promise.resolve();
            return runTask(app.update_source === 'mirrorchyan' ? 'mirror_configure' : 'git_configure', 'setup_app', {appName: app.name, profileName}, `Initiating profile change for '${app.name}' to '${profileName}'...`);
        },
        update(version: string, notes?: string) {
            const app = state.app;
            if (!app || isTaskActive(state.task) || state.request || app.running) return Promise.resolve();
            const kind = app.update_source === 'git'
                ? installedSource(app) === 'mirrorchyan' ? 'git_install' : 'git_update'
                : app.installed && installedSource(app) === 'mirrorchyan' ? 'mirror_update' : 'mirror_install';
            const action = getVersionActionType(version, app.current_version);
            if (kind.endsWith('_update')) void transport.invoke('send_notification_cmd', {title: `${t(action)}: ${app.name}`, body: notes ? `${version}\n${notes}` : version}).catch(console.warn);
            return runTask(kind, 'update_to_version', {appName: app.name, version}, kind.endsWith('_update') ? `Initiating ${action} for '${app.name}' to version '${version}'...` : t('Installing App: {{appName}}', {appName: app.name}), notes);
        },
        async cancel() {
            const task = state.task;
            if (!task?.can_cancel || !isTaskActive(task) || task.status === 'cancelling') return;
            patch({task: {...task, status: 'cancelling'}});
            try {
                const accepted = await transport.invoke<boolean>('cancel_app_operation', {appName: task.app_name, operationId: task.id});
                if (!accepted && state.task?.id === task.id && state.task.status === 'cancelling' && state.task.sequence === task.sequence) patch({task});
            } catch (error) {
                if (state.task?.id === task.id && isTaskActive(state.task) && state.task.sequence === task.sequence) patch({task});
                updateStatus({error: `Cancel operation for ${task.app_name} failed: ${errorText(error)}`});
            }
        },
        async setting<T>(operation: () => Promise<T>): Promise<T> {
            if (state.request || isTaskActive(state.task)) throw new Error(t('Process in progress...'));
            const generation = ++requestGeneration;
            patch({request: 'settings'});
            try {return await operation();}
            finally {if (!disposed && generation === requestGeneration) patch({request: null});}
        },
        refresh: () => request('refresh', 'load_app', undefined, 'Failed to check for updates', () => updateStatus({info: t('App Refreshed.')})),
        start() {
            const app = state.app; if (!app) return Promise.resolve();
            if (state.request || isTaskActive(state.task)) return Promise.resolve();
            patch({applicationOutcome: undefined, applicationLogs: [{app_name: app.name, message: `Starting App: ${app.name}`}]});
            return request('start', 'start_app', {appName: app.name}, `Start app ${app.name} failed`);
        },
        stop: () => request('stop', 'stop_app', {appName: state.app?.name}, `Stop app ${state.app?.name} failed`),
        delete: () => request('delete', 'delete_app', {appName: state.app?.name}, `Delete app ${state.app?.name} failed`, () => patch({task: undefined, proposal: null})),
        preferences: (updateMethod: string | null, autoStart: boolean | null) => request('preferences', 'update_app_preferences', {appName: state.app?.name, updateMethod, autoStart}, `Failed to update ${state.app?.name}`, () => updateStatus({info: t('App settings updated successfully.')})),
        defender: () => request('defender', 'add_defender_exclusion', {appName: state.app?.name}, t('failedToAddExclusion', {errorMessage: ''}), () => {patch({defenderHidden: true}); updateStatus({info: t('defenderExclusionAdded', {appName: state.app?.name})});}),
    };
}
export type LauncherController = ReturnType<typeof createLauncherController>;
