import {useTranslation} from 'react-i18next';
import ConsolePage, {type MessagePayload} from './ConsolePage';
import {calculateVersionChangeProgress, type VersionActionType} from './updateProgress';

interface GitConsoleProps {
    appName: string;
    logs: MessagePayload[];
    isProcessing: boolean;
    onBack: () => void;
    onCancel: () => Promise<void>;
}

export function GitInstallConsole(props: GitConsoleProps) {
    const {t} = useTranslation();
    return <ConsolePage {...props} title={t('Installing App: {{appName}}', {appName: props.appName})}/>;
}

export function GitUpdateConsole({actionType, ...props}: GitConsoleProps & {actionType: VersionActionType}) {
    const {t} = useTranslation();
    return <ConsolePage
        {...props}
        inline
        title={t('{{actionType}} App: {{appName}}', {actionType: t(actionType), appName: props.appName})}
        progress={calculateVersionChangeProgress(props.logs, props.isProcessing)}
        progressAction={t(actionType)}
    />;
}
