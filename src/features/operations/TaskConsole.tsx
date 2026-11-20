import {useTranslation} from 'react-i18next';
import ConsolePage from '../../ConsolePage';
import {calculateVersionChangeProgress} from '../../updateProgress';
import type {Task} from '../../types';
import {isConfiguration, isTaskActive, isUpdate} from './model';

import {mirrorPhaseLabel} from '../mirror/progress';

export default function TaskConsole({task, inline = false, onBack, onCancel}: {
    task: Task; inline?: boolean; onBack: () => void; onCancel: () => Promise<void>;
}) {
    const {t} = useTranslation();
    const active = isTaskActive(task);
    const configure = isConfiguration(task);
    const action = configure ? 'Change Profile' : isUpdate(task) ? task.action : 'Install';
    const failed = task.status === 'failed';
    const completed = task.status === 'succeeded';
    const title = configure
        ? t("Changing Profile: {{appName}} to '{{newProfile}}'", {appName: task.app_name, newProfile: task.profile})
        : isUpdate(task) ? t('{{actionType}} App: {{appName}}', {actionType: t(task.action), appName: task.app_name})
        : t('Installing App: {{appName}}', {appName: task.app_name});
    let progress = inline || isUpdate(task) || task.kind === 'mirror_install'
        ? calculateVersionChangeProgress(task.logs.map(log => ({...log, finished: false})), active)
        : undefined;
    if (progress) {
        progress = {...progress, phase: failed ? 'failed' : completed ? 'complete' : progress.phase, value: completed ? 100 : progress.value};
        if (task.kind.startsWith('mirror_')) {
            const phase = task.progress?.phase ?? 'preparing';
            const known = (phase === 'downloading' || phase === 'extracting') && !!task.progress?.total;
            progress = {
                value: completed ? 100 : task.progress?.value ?? 0,
                phase: failed ? 'failed' : completed ? 'complete' : 'preparing', requirementsValue: null,
                indeterminate: active && !known && !task.progress?.value,
                phaseLabel: configure ? t(failed ? 'Profile change failed.' : completed ? 'Profile change completed.' : 'Change Profile')
                    : isUpdate(task) && (failed || completed) ? t(`${task.action} ${failed ? 'failed' : 'completed'}`)
                    : t(mirrorPhaseLabel(failed ? 'install_failed' : completed ? 'completed' : phase)),
            };
        }
    }
    return <ConsolePage title={title} appName={task.app_name} logs={task.logs} inline={inline}
        isProcessing={active} outcome={task.status} cancelPending={task.status === 'cancelling'}
        onBack={onBack} onCancel={task.can_cancel ? onCancel : undefined} progress={progress} progressAction={t(action)}/>;
}
