import {useTranslation} from 'react-i18next';
import ConsolePage, {type MessagePayload} from './ConsolePage';

export interface MirrorProgress {
    phase: string;
    downloaded: number;
    total: number | null;
}

export const mirrorPhaseLabel = (phase: string): string => ({
    downloading: 'Downloading update...',
    downloaded: 'Update downloaded and verified.',
    extracting: 'Extracting update...',
    installing: 'Installing application...',
    completed: 'Installation completed.',
    download_failed: 'Update download failed.',
    install_failed: 'Installation failed.',
    cancelled: 'Operation cancelled.',
} as Record<string, string>)[phase] ?? 'Preparing installation...';

interface MirrorConsoleProps {
    appName: string;
    logs: MessagePayload[];
    isProcessing: boolean;
    failed: boolean;
    progress?: MirrorProgress;
    onBack: () => void;
    onCancel: () => Promise<void>;
}

export default function MirrorOperationConsole({progress, failed, ...props}: MirrorConsoleProps) {
    const {t} = useTranslation();
    const finished = [...props.logs].reverse().find(log => log.finished);
    const phase = progress?.phase ?? 'preparing';
    const hasFailed = failed || phase.endsWith('_failed') || !!finished?.error;
    const cancelled = phase === 'cancelled' || !!finished?.cancelled;
    const completed = !hasFailed && !cancelled && (phase === 'completed' || !!finished);
    const downloadKnown = phase === 'downloading' && !!progress?.total;
    return <ConsolePage
        {...props}
        inline
        title={t('Installing App: {{appName}}', {appName: props.appName})}
        progressAction={t('Install')}
        progress={{
            value: completed ? 100 : downloadKnown
                ? Math.min(100, Math.round(progress!.downloaded / progress!.total! * 100)) : 0,
            phase: hasFailed ? 'failed' : completed ? 'complete' : 'preparing',
            requirementsValue: null,
            indeterminate: !hasFailed && !completed && !downloadKnown && !cancelled,
            phaseLabel: t(mirrorPhaseLabel(hasFailed ? 'install_failed' : completed ? 'completed' : phase)),
        }}
    />;
}
