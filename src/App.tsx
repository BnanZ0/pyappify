// src/App.tsx
import './i18n';
import {useCallback, useEffect, useMemo, useRef, useState} from "react";
import {invoke} from "@tauri-apps/api/core";
import {listen, UnlistenFn} from "@tauri-apps/api/event";
import {getVersion} from '@tauri-apps/api/app';
import {getCurrentWindow} from '@tauri-apps/api/window';
import {openUrl} from '@tauri-apps/plugin-opener';
import UpdateLogPage from "./UpdateLogPage";
import ConsolePage, {type MessagePayload} from "./ConsolePage.tsx";
import SettingsPage from "./SettingsPage.tsx";
import {type VersionActionType} from "./updateProgress";
import {GitInstallConsole, GitUpdateConsole} from './GitOperationConsole';
import MirrorOperationConsole, {mirrorPhaseLabel} from './MirrorOperationConsole';

import {
    Alert,
    Box,
    Button,
    Card,
    CardContent,
    Chip,
    CircularProgress,
    Container,
    Dialog,
    DialogActions,
    DialogContent,
    DialogContentText,
    DialogTitle,
    FormControl,
    FormControlLabel,
    IconButton,
    InputLabel,
    Link,
    MenuItem,
    Select,
    Snackbar,
    Stack,
    Switch,
    Tooltip,
    Typography
} from "@mui/material";
import {
    Build,
    Cached,
    Delete,
    KeyboardArrowRight,
    OpenInNew,
    PlayArrow,
    Settings as SettingsIcon,
    StopCircle,
} from '@mui/icons-material';
import {alpha, createTheme, ThemeProvider} from '@mui/material/styles';
import CssBaseline from '@mui/material/CssBaseline';
import useMediaQuery from '@mui/material/useMediaQuery';
import {useTranslation} from 'react-i18next';
import {invokeTauriCommandWrapper} from "./utils.ts";

interface Profile {
    name: string;
    main_script: string;
    admin: boolean;
    requirements: string;
    python_path: string;
}

interface App {
    name: string;
    update_source: 'git' | 'mirrorchyan';
    mirrorchyan: {resource_id: string; prerelease_channel?: string | null} | null;
    icon: string;
    website: string | null;
    path: string;
    current_version: string | null;
    available_versions: string[];
    running: boolean;
    installed: boolean;
    installation?: {source: 'git' | 'mirrorchyan'; version: string | null} | null;
    update_method: string;
    auto_start: boolean;
    update_state: 'idle' | 'updating' | 'failed';
    update_target_version: string | null;
    update_error: string | null;
    profiles: Profile[];
    current_profile: string;
    show_add_defender: boolean;
}

type ParsedVersion = {
    major: number;
    minor: number;
    patch: number;
    prerelease: null | {
        stage: 'alpha' | 'beta' | 'rc';
        number: number | null;
    };
};

const parseVersion = (version: string): ParsedVersion | null => {
    const match = version.match(/^v?(\d+)\.(\d+)\.(\d+)(?:(?:-|\.)(alpha|beta|rc)(?:\.(\d+))?)?$/);
    if (!match) return null;
    return {
        major: Number(match[1]),
        minor: Number(match[2]),
        patch: Number(match[3]),
        prerelease: match[4]
            ? {stage: match[4] as 'alpha' | 'beta' | 'rc', number: match[5] ? Number(match[5]) : null}
            : null,
    };
};

const prereleaseRank = (version: ParsedVersion): number => {
    if (!version.prerelease) return 3;
    if (version.prerelease.stage === 'rc') return 2;
    if (version.prerelease.stage === 'beta') return 1;
    return 0;
};

const compareVersions = (v1: string, v2: string): number => {
    const left = parseVersion(v1);
    const right = parseVersion(v2);
    if (!left || !right) return v1.localeCompare(v2, undefined, {numeric: true, sensitivity: 'base'});

    const numericParts: Array<keyof Pick<ParsedVersion, 'major' | 'minor' | 'patch'>> = ['major', 'minor', 'patch'];
    for (const part of numericParts) {
        if (left[part] !== right[part]) return left[part] - right[part];
    }

    const rankDiff = prereleaseRank(left) - prereleaseRank(right);
    if (rankDiff !== 0) return rankDiff;

    const leftNumber = left.prerelease?.number ?? -1;
    const rightNumber = right.prerelease?.number ?? -1;
    return leftNumber - rightNumber;
};

const isReleaseVersion = (version: string): boolean => parseVersion(version)?.prerelease === null;

const getVersionChannelLabelKey = (version: string): string => (
    isReleaseVersion(version) ? 'Release Version' : 'Test Version'
);

const getVersionActionType = (
    targetVersion: string,
    currentVersion: string | null,
    sameVersionAction: VersionActionType = 'Set',
): VersionActionType => {
    if (!targetVersion) return sameVersionAction;
    if (!currentVersion) return sameVersionAction;
    const comparison = compareVersions(targetVersion, currentVersion);
    return comparison > 0 ? 'Upgrade' : comparison < 0 ? 'Downgrade' : sameVersionAction;
};

const getVersionActionProgressKey = (actionType: VersionActionType): string => {
    if (actionType === 'Upgrade') return 'Upgrading...';
    if (actionType === 'Downgrade') return 'Downgrading...';
    return 'Setting...';
};

type StatusState = {
    loading?: boolean;
    error?: string | null;
    info?: string | null;
    messageLoading?: boolean;
};

const UPDATE_METHOD_OPTIONS = [
    'MANUAL_UPDATE',
    'AUTO_UPDATE',
    'AUTO_UPDATE_PRE_RELEASE',
] as const;

type Page =
    'list'
    | 'installConsole'
    | 'runningAppConsole'
    | 'settings'
    | 'profileChooser'
    | 'changeProfile'
    | 'profileChangeConsole';

type InlineUpdateLogState = {
    version: string;
    actionType: VersionActionType;
    isConfirming: boolean;
    completed?: boolean;
    failed?: boolean;
};

type InlineConsoleKind = 'start' | 'git-update' | 'mirror';

const MAX_CONSOLE_LOGS = 500;
const CONSOLE_LOG_STORAGE_KEY = 'pyappifyConsoleLogs';

const loadConsoleLogs = (): Record<string, MessagePayload[]> => {
    try {
        const stored = localStorage.getItem(CONSOLE_LOG_STORAGE_KEY);
        if (!stored) return {};
        const parsed = JSON.parse(stored);
        return parsed && typeof parsed === 'object' ? parsed : {};
    } catch (error) {
        console.warn('Failed to restore console logs:', error);
        return {};
    }
};

type AppIconAsset = {
    bytes: number[];
    mime_type: string;
};

function AppIcon({appName, iconPath}: {appName: string; iconPath: string}) {
    const [src, setSrc] = useState<string | null>(null);

    useEffect(() => {
        let disposed = false;
        let objectUrl: string | null = null;
        setSrc(null);

        if (!iconPath?.trim()) return;

        invoke<AppIconAsset | null>('get_app_icon', {appName})
            .then((asset) => {
                if (!asset) return;
                objectUrl = URL.createObjectURL(new Blob([new Uint8Array(asset.bytes)], {type: asset.mime_type}));
                if (disposed) {
                    URL.revokeObjectURL(objectUrl);
                } else {
                    setSrc(objectUrl);
                }
            })
            .catch(() => {
                if (!disposed) setSrc(null);
            });

        return () => {
            disposed = true;
            if (objectUrl) URL.revokeObjectURL(objectUrl);
        };
    }, [appName, iconPath]);

    if (!src) return null;

    return (
        <Box
            component="img"
            src={src}
            alt=""
            sx={{width: 46, height: 46, objectFit: 'contain', flexShrink: 0}}
        />
    );
}

export type ThemeModeSetting = 'light' | 'dark' | 'system';

function App() {
    const {t} = useTranslation();
    const [app, setApp] = useState<App | null>(null);
    const mirrorUpdateProgressRef = useRef<Record<string, string>>({});
    const [mirrorUpdateProgress, setMirrorUpdateProgress] = useState<Record<string, {phase: string; downloaded: number; total: number | null}>>({});
    const [status, setStatus] = useState<StatusState>({loading: true, error: null, info: null, messageLoading: false});
    const [appActionLoading, setAppActionLoading] = useState<Record<string, boolean>>({});
    const [selectedTargetVersions, setSelectedTargetVersions] = useState<Record<string, string>>({});
    const selectedTargetVersionsRef = useRef(selectedTargetVersions);
    const [currentPage, setCurrentPage] = useState<Page>('list');
    // Console state remains keyed by app name so persisted logs stay compatible.
    const [inlineUpdateLogs, setInlineUpdateLogs] = useState<Record<string, InlineUpdateLogState>>({});
    const [inlineConsoles, setInlineConsoles] = useState<Record<string, InlineConsoleKind>>({});
    // Updated synchronously so the app event listener sees it before React re-renders.
    const completedAppsRef = useRef<Set<string>>(new Set());
    const appUpdateStatesRef = useRef<Record<string, App['update_state']>>({});
    const appSourcesRef = useRef<Record<string, App['update_source']>>({});
    const activeGitUpdateAppsRef = useRef<Set<string>>(new Set());
    const gitInstallAppsRef = useRef<Set<string>>(new Set());
    const activeMirrorAppsRef = useRef<Set<string>>(new Set());
    const [isMirrorProcessRunning, setIsMirrorProcessRunning] = useState(false);
    const [isGitInstallProcessRunning, setIsGitInstallProcessRunning] = useState<boolean>(false);
    const [isStartAppProcessRunning, setIsStartAppProcessRunning] = useState<boolean>(false);
    const [startingAppName, setStartingAppName] = useState<string | null>(null);
    const [consoleLogs, setConsoleLogs] = useState<Record<string, MessagePayload[]>>(loadConsoleLogs);
    const [isRunningAppConsoleOpen, setIsRunningAppConsoleOpen] = useState<boolean>(false);
    const [themeMode, setThemeMode] = useState<ThemeModeSetting>(() => {
        const savedTheme = localStorage.getItem('appThemeMode');
        if (savedTheme === 'light' || savedTheme === 'dark' || savedTheme === 'system') {
            return savedTheme as ThemeModeSetting;
        }
        return 'system';
    });
    const [profileChoiceApp, setProfileChoiceApp] = useState<App | null>(null);
    const [selectedProfileForInstall, setSelectedProfileForInstall] = useState<string>("");
    const [appForProfileChange, setAppForProfileChange] = useState<App | null>(null);
    const [selectedNewProfileName, setSelectedNewProfileName] = useState<string>("");
    const [isProfileChangeProcessRunning, setIsProfileChangeProcessRunning] = useState<boolean>(false);
    const [profileChangeData, setProfileChangeData] = useState<{ appName: string; newProfile: string } | null>(null);
    const [isConfirmDeleteDialogOpen, setConfirmDeleteDialogOpen] = useState(false);
    const [appToDelete, setAppToDelete] = useState<string | null>(null);
    const [checkingUpdateForApp, setCheckingUpdateForApp] = useState<string | null>(null);
    const [appVersion, setAppVersion] = useState('');
    const [hiddenDefenderButtons, setHiddenDefenderButtons] = useState<Set<string>>(new Set());
    const [addingDefenderExclusionForApp, setAddingDefenderExclusionForApp] = useState<string | null>(null);
    const [snackbarOpen, setSnackbarOpen] = useState(false);
    const [snackbarMessage, setSnackbarMessage] = useState("");
    const [snackbarSeverity, setSnackbarSeverity] = useState<"success" | "info" | "warning" | "error">("info");

    const handleOpenWebsite = useCallback(async () => {
        const website = app?.website?.trim();
        if (!website) return;
        try {
            await openUrl(website);
        } catch (error) {
            console.warn('Failed to open app website:', error);
        }
    }, [app?.website]);

    useEffect(() => {
        selectedTargetVersionsRef.current = selectedTargetVersions;
    }, [selectedTargetVersions]);

    useEffect(() => {
        localStorage.setItem('appThemeMode', themeMode);
    }, [themeMode]);

    useEffect(() => {
        if (!app?.name) return;
        const launcherName = t('appLauncherName', {appName: app.name});
        document.title = launcherName;
        getCurrentWindow().setTitle(launcherName).catch((error) => {
            console.warn('Failed to update launcher window title:', error);
        });
    }, [app?.name, t]);

    useEffect(() => {
        try {
            localStorage.setItem(CONSOLE_LOG_STORAGE_KEY, JSON.stringify(consoleLogs));
        } catch (error) {
            console.warn('Failed to persist console logs:', error);
        }
    }, [consoleLogs]);

    const addConsoleLog = useCallback((logEntry: MessagePayload) => {
        setConsoleLogs(previous => {
            const appLogs = previous[logEntry.app_name] ?? [];
            let nextAppLogs: MessagePayload[];
            if (logEntry.update && appLogs.length > 0) {
                nextAppLogs = [...appLogs];
                nextAppLogs[nextAppLogs.length - 1] = logEntry;
            } else {
                nextAppLogs = [...appLogs, logEntry];
            }
            if (nextAppLogs.length > MAX_CONSOLE_LOGS) {
                nextAppLogs = nextAppLogs.slice(-MAX_CONSOLE_LOGS);
            }
            return {...previous, [logEntry.app_name]: nextAppLogs};
        });
    }, []);

    const beginConsoleSession = useCallback((appName: string, message: string) => {
        setConsoleLogs(previous => ({
            ...previous,
            [appName]: [{message, app_name: appName}],
        }));
    }, []);

    const ensureActiveConsoleSession = useCallback((appName: string, message: string) => {
        setConsoleLogs(previous => {
            const existingLogs = previous[appName] ?? [];
            const hasActiveSession = existingLogs.length > 0 && !existingLogs.some(log => log.finished);
            return hasActiveSession
                ? previous
                : {...previous, [appName]: [{message, app_name: appName}]};
        });
    }, []);

    const prefersDarkMode = useMediaQuery('(prefers-color-scheme: dark)');
    const muiTheme = useMemo(() => {
        const mode: 'light' | 'dark' = themeMode === 'system' ? (prefersDarkMode ? 'dark' : 'light') : themeMode;
        const isDark = mode === 'dark';
        return createTheme({
            palette: {
                mode,
                primary: {main: '#6366f1'},
                success: {main: isDark ? '#34d399' : '#059669'},
                warning: {main: isDark ? '#fbbf24' : '#d97706'},
                background: {
                    default: isDark ? '#0b0f17' : '#f4f6fb',
                    paper: isDark ? '#121824' : '#ffffff',
                },
                divider: isDark ? 'rgba(148, 163, 184, 0.16)' : 'rgba(15, 23, 42, 0.10)',
            },
            shape: {borderRadius: 12},
            typography: {
                fontFamily: 'Inter, "Segoe UI", system-ui, sans-serif',
                h5: {fontWeight: 720, letterSpacing: '-0.025em'},
                h6: {fontWeight: 700, letterSpacing: '-0.015em'},
                button: {fontWeight: 650, letterSpacing: 0, textTransform: 'none'},
            },
            components: {
                MuiButton: {
                    defaultProps: {disableElevation: true},
                    styleOverrides: {root: {borderRadius: 10, minHeight: 38}},
                },
                MuiCard: {
                    styleOverrides: {
                        root: {
                            borderRadius: 18,
                            boxShadow: isDark
                                ? '0 18px 50px rgba(0, 0, 0, 0.28)'
                                : '0 18px 50px rgba(30, 41, 59, 0.07)',
                        },
                    },
                },
                MuiFormControl: {
                    styleOverrides: {root: {'& .MuiOutlinedInput-root': {borderRadius: 10}}},
                },
                MuiTooltip: {defaultProps: {arrow: true}},
            },
        });
    }, [themeMode, prefersDarkMode]);

    const updateStatus = useCallback((newStatus: Partial<StatusState>) => {
        setStatus(prevStatus => ({...prevStatus, ...newStatus}));
    }, []);

    const clearMessages = useCallback(() => {
        updateStatus({error: null, info: null});
    }, [updateStatus]);

    const handleAppPreferenceChange = async (
        appName: string,
        updateMethod: string | null,
        autoStart: boolean | null,
    ) => {
        clearMessages();
        updateStatus({messageLoading: true});
        await invokeTauriCommandWrapper<void>(
            'update_app_preferences',
            {appName, updateMethod, autoStart},
            () => updateStatus({info: t('App settings updated successfully.'), messageLoading: false}),
            (errorMessage) => updateStatus({error: `Failed to update ${appName}: ${errorMessage}`, messageLoading: false}),
        );
    };

    const handleStartApp = async (appName: string) => {
        clearMessages();
        setAppActionLoading(prev => ({...prev, [appName]: true}));
        setStartingAppName(appName);
        beginConsoleSession(appName, `Attempting to start app: ${appName}...`);
        setIsStartAppProcessRunning(true);
        setInlineConsoles(prev => ({...prev, [appName]: 'start'}));

        await invokeTauriCommandWrapper<void>("start_app", {appName}, () => {},
            (errorMessage, rawError) => {
                console.error(`Failed to start app ${appName}:`, rawError);
                addConsoleLog({message: `ERROR: Failed to start app: ${errorMessage}`, app_name: appName, error: true, finished: true});
                setIsStartAppProcessRunning(false);
            }
        );
    };

    const ensureMirrorCdk = async (): Promise<boolean> => {
        try {
            if (await invoke<boolean>('mirrorchyan_has_cdk')) return true;
        } catch {
            // An unreadable saved key is configured again in the same settings flow.
        }
        setCurrentPage('settings');
        updateStatus({info: t('Configure a MirrorChyan CDK in Settings before installing or updating.'), error: null});
        return false;
    };

    const handleGitInstallWithProfile = async (appName: string, profileName: string) => {
        clearMessages();
        activeGitUpdateAppsRef.current.delete(appName);
        completedAppsRef.current.delete(appName);
        setInlineConsoles(prev => {
            const next = {...prev};
            delete next[appName];
            return next;
        });
        setInlineUpdateLogs(prev => {
            const next = {...prev};
            delete next[appName];
            return next;
        });
        setAppActionLoading(prev => ({...prev, [appName]: true}));
        setStartingAppName(appName);
        beginConsoleSession(appName, `Initiating install for '${appName}' with profile '${profileName}'...`);
        gitInstallAppsRef.current.add(appName);
        setIsGitInstallProcessRunning(true);
        setCurrentPage('installConsole');

        await invokeTauriCommandWrapper<void>("setup_app", {appName, profileName}, () => {},
            (errorMessage, rawError) => {
                console.error(`Failed to invoke setup_app for ${appName} with profile ${profileName}:`, rawError);
                addConsoleLog({message: `ERROR: Failed to install app: ${errorMessage}`, app_name: appName, error: true, finished: true});
                gitInstallAppsRef.current.delete(appName);
                setIsGitInstallProcessRunning(false);
                setAppActionLoading(prev => ({...prev, [appName]: false}));
            }
        );
    };

    const beginMirrorOperation = (appName: string) => {
        clearMessages();
        completedAppsRef.current.delete(appName);
        activeMirrorAppsRef.current.add(appName);
        delete mirrorUpdateProgressRef.current[appName];
        setMirrorUpdateProgress(prev => ({...prev, [appName]: {phase: 'preparing', downloaded: 0, total: null}}));
        setAppActionLoading(prev => ({...prev, [appName]: true}));
        beginConsoleSession(appName, t('Installing App: {{appName}}', {appName}));
        setIsMirrorProcessRunning(true);
        setInlineConsoles(prev => ({...prev, [appName]: 'mirror'}));
        setCurrentPage('list');
    };

    const finishMirrorInvocationError = (appName: string, errorMessage: string) => {
        activeMirrorAppsRef.current.delete(appName);
        setIsMirrorProcessRunning(false);
        setAppActionLoading(prev => ({...prev, [appName]: false}));
        addConsoleLog({message: errorMessage, app_name: appName, error: true, finished: true});
    };

    const handleMirrorInstall = async (mirrorApp: App) => {
        if (!mirrorApp.installed && !await ensureMirrorCdk()) return;
        setInlineUpdateLogs(prev => {
            const next = {...prev};
            delete next[mirrorApp.name];
            return next;
        });
        beginMirrorOperation(mirrorApp.name);
        await invokeTauriCommandWrapper<void>('setup_app', {
            appName: mirrorApp.name,
            profileName: mirrorApp.current_profile || mirrorApp.profiles?.[0]?.name || 'default',
        }, () => {}, errorMessage => finishMirrorInvocationError(mirrorApp.name, errorMessage));
    };

    const handleInstallClick = (app: App) => {
        if (app.update_source === 'mirrorchyan') {
            void handleMirrorInstall(app);
            return;
        }
        if (app.profiles && app.profiles.length > 1) {
            setProfileChoiceApp(app);
            const initialProfile = app.profiles.some(p => p.name === app.current_profile)
                ? app.current_profile
                : app.profiles[0]?.name || "default";
            setSelectedProfileForInstall(initialProfile);
            setCurrentPage('profileChooser');
        } else {
            const profileName = app.current_profile || app.profiles?.[0]?.name || "default";
            void handleGitInstallWithProfile(app.name, profileName);
        }
    };

    useEffect(() => {
        getVersion().then(setAppVersion);
    }, []);

    useEffect(() => {
        const unlistenPromises: Promise<UnlistenFn>[] = [];
        unlistenPromises.push(listen<{app_name: string; phase: string; downloaded: number; total: number | null}>('mirror-update-progress', event => {
            const {app_name, phase, downloaded, total} = event.payload;
            setMirrorUpdateProgress(prev => ({...prev, [app_name]: {phase, downloaded, total}}));
            // Keep progress in the original log view without flooding it with every chunk.
            const percent = total ? Math.floor(downloaded / total * 10) * 10 : 0;
            const key = phase === 'downloading' ? `${phase}:${percent}` : phase;
            if (mirrorUpdateProgressRef.current[app_name] === key) return;
            mirrorUpdateProgressRef.current[app_name] = key;
            const detail = phase === 'downloading' && total
                ? ` ${(downloaded / 1024 / 1024).toFixed(1)} MB / ${(total / 1024 / 1024).toFixed(1)} MB`
                : '';
            addConsoleLog({app_name, message: t(mirrorPhaseLabel(phase)) + detail, error: phase.endsWith('_failed')});
        }));
        invoke('show_main_window').then();

        unlistenPromises.push(listen<App>("app", (event) => {
            const app = event.payload;
            const previousSource = appSourcesRef.current[app.name];
            if (previousSource && previousSource !== app.update_source) {
                activeGitUpdateAppsRef.current.delete(app.name);
                activeMirrorAppsRef.current.delete(app.name);
                gitInstallAppsRef.current.delete(app.name);
                completedAppsRef.current.delete(app.name);
                setIsGitInstallProcessRunning(false);
                setIsMirrorProcessRunning(false);
                setInlineConsoles(prev => {
                    const next = {...prev};
                    delete next[app.name];
                    return next;
                });
                setInlineUpdateLogs(prev => {
                    const next = {...prev};
                    delete next[app.name];
                    return next;
                });
            }
            appSourcesRef.current[app.name] = app.update_source;
            const previousUpdateState = appUpdateStatesRef.current[app.name];
            if (app.update_state === 'updating' && previousUpdateState !== 'updating') {
                if (app.update_source === 'mirrorchyan') {
                    activeMirrorAppsRef.current.add(app.name);
                    setInlineConsoles(prev => ({...prev, [app.name]: 'mirror'}));
                    setMirrorUpdateProgress(prev => ({...prev, [app.name]: {phase: 'preparing', downloaded: 0, total: null}}));
                    setIsMirrorProcessRunning(true);
                    ensureActiveConsoleSession(app.name, t('Installing App: {{appName}}', {appName: app.name}));
                } else if (!app.installed) {
                    // Installing the selected Git source is not a Git version update.
                    gitInstallAppsRef.current.add(app.name);
                    setIsGitInstallProcessRunning(true);
                    setInlineConsoles(prev => {
                        const next = {...prev};
                        delete next[app.name];
                        return next;
                    });
                    ensureActiveConsoleSession(app.name, t('Installing App: {{appName}}', {appName: app.name}));
                } else {
                    activeGitUpdateAppsRef.current.add(app.name);
                    setInlineConsoles(prev => ({...prev, [app.name]: 'git-update'}));
                    const actionType = getVersionActionType(
                        app.update_target_version ?? '',
                        app.current_version,
                        'Upgrade',
                    );
                    ensureActiveConsoleSession(
                        app.name,
                        `${getVersionActionProgressKey(actionType)} '${app.name}' to version '${app.update_target_version ?? 'unknown'}'`,
                    );
                }
            }
            appUpdateStatesRef.current[app.name] = app.update_state;
            setApp(app);
            const newSelectedTargets: Record<string, string> = {};
            const inlineLogUpdates: Record<string, InlineUpdateLogState> = {};
            if (app.installed && app.update_state !== 'idle' && app.update_target_version) {
                const actionType = getVersionActionType(
                    app.update_target_version,
                    app.current_version,
                    'Upgrade',
                );
                newSelectedTargets[app.name] = app.update_target_version;
                inlineLogUpdates[app.name] = {
                    version: app.update_target_version,
                    actionType,
                    isConfirming: app.update_state === 'updating',
                    completed: false,
                    failed: app.update_state === 'failed',
                };
            } else if (!app.installed || app.running) {
                if (selectedTargetVersionsRef.current[app.name]) newSelectedTargets[app.name] = '';
            } else if (!completedAppsRef.current.has(app.name)) {
                const sortedVersions = [...app.available_versions]
                    .filter(isReleaseVersion)
                    .sort((a, b) => compareVersions(b, a));
                const latestVersion = sortedVersions[0];
                const currentSelection = selectedTargetVersionsRef.current[app.name];
                if (app.current_version && latestVersion && compareVersions(latestVersion, app.current_version) > 0) {
                    newSelectedTargets[app.name] = latestVersion;
                    // Auto-select latest version: show update log inline
                    inlineLogUpdates[app.name] = {
                        version: latestVersion,
                        actionType: 'Upgrade',
                        isConfirming: false,
                    };
                } else if (currentSelection && app.available_versions.includes(currentSelection) && currentSelection !== app.current_version) {
                    newSelectedTargets[app.name] = currentSelection;
                } else {
                    newSelectedTargets[app.name] = '';
                }
            }
            setSelectedTargetVersions(prev => ({...prev, ...newSelectedTargets}));
            // Preserve the result of the most recent attempt while app data refreshes.
            setInlineUpdateLogs(prev => {
                const merged = {...prev};
                for (const [name, entry] of Object.entries(inlineLogUpdates)) {
                    if (entry.isConfirming || entry.failed) {
                        completedAppsRef.current.delete(name);
                        merged[name] = entry;
                    } else if (!completedAppsRef.current.has(name) && !merged[name]?.failed) {
                        merged[name] = entry;
                    }
                }
                return merged;
            });
            updateStatus({loading: false});
        }));

        unlistenPromises.push(listen<App>("choose_app_profile", (event) => {
            const app = event.payload;
            setProfileChoiceApp(app);
            const initialProfile = app.profiles?.some(p => p.name === app.current_profile)
                ? app.current_profile
                : app.profiles?.[0]?.name || "default";
            setSelectedProfileForInstall(initialProfile);
            setCurrentPage('profileChooser');
        }));
        unlistenPromises.push(listen<{
            app_name: string;
            message: string;
            finished?: boolean;
            error?: boolean;
            cancelled?: boolean;
        }>("app-log", (event) => {
            const {app_name, finished, error, cancelled} = event.payload;
            addConsoleLog(event.payload);
            const isGitUpdateEvent = activeGitUpdateAppsRef.current.has(app_name);
            if (isGitUpdateEvent) {
                setInlineUpdateLogs(prev => {
                    const entry = prev[app_name];
                    if (!entry || entry.completed) return prev;
                    if (finished) {
                        if (cancelled) {
                            completedAppsRef.current.delete(app_name);
                            const next = {...prev};
                            delete next[app_name];
                            return next;
                        } else if (error) {
                            completedAppsRef.current.delete(app_name); // failed — not completed
                            return {...prev, [app_name]: {...entry, isConfirming: false, failed: true}};
                        } else {
                            completedAppsRef.current.add(app_name); // mark completed immediately
                            return {...prev, [app_name]: {...entry, isConfirming: false, completed: true, failed: false}};
                        }
                    } else if (!entry.isConfirming) {
                        return {...prev, [app_name]: {...entry, isConfirming: true, failed: false}};
                    }
                    return prev;
                });
                if (finished && !error) {
                    setSelectedTargetVersions(prev => ({...prev, [app_name]: ''}));
                }
                if (finished) activeGitUpdateAppsRef.current.delete(app_name);
            }
            if (finished) {
                if (gitInstallAppsRef.current.delete(app_name)) setIsGitInstallProcessRunning(false);
                if (activeMirrorAppsRef.current.delete(app_name)) {
                    setIsMirrorProcessRunning(false);
                    setInlineUpdateLogs(prev => {
                        const entry = prev[app_name];
                        if (!entry) return prev;
                        if (cancelled) {
                            const next = {...prev};
                            delete next[app_name];
                            return next;
                        }
                        return {...prev, [app_name]: {...entry,
                            isConfirming: false, failed: !!error, completed: !error,
                        }};
                    });
                    if (!error) {
                        completedAppsRef.current.add(app_name);
                        setSelectedTargetVersions(prev => ({...prev, [app_name]: ''}));
                    }
                }
                setAppActionLoading(prev => ({...prev, [app_name]: false}));
                setIsStartAppProcessRunning(false);
                setIsRunningAppConsoleOpen(false);
                setIsProfileChangeProcessRunning(false);
            }
        }));
        unlistenPromises.push(listen<string>('mirrorchyan-cdk-required', () => {
            updateStatus({info: t('Configure a MirrorChyan CDK in Settings to enable automatic updates.')});
        }));

        (async () => {
            await invokeTauriCommandWrapper<App>("load_app", undefined, () => {},
                (errorMessage, rawError) => {
                    console.error("Failed to initially load app:", rawError);
                    updateStatus({error: `Failed to load app: ${errorMessage}`, loading: false});
                }
            );
        })();

        return () => {
            Promise.all(unlistenPromises).then(unlisteners => unlisteners.forEach(fn => fn()));
        };
    }, [addConsoleLog, ensureActiveConsoleSession, updateStatus, t]);

    const handleDeleteApp = async (appName: string) => {
        clearMessages();
        updateStatus({messageLoading: true});
        setAppActionLoading(prev => ({...prev, [appName]: true}));

        await invokeTauriCommandWrapper<void>("delete_app", {appName}, () => {},
            (errorMessage, rawError) => {
                console.error(`Failed to delete app ${appName}:`, rawError);
                updateStatus({error: `Delete app ${appName} failed: ${errorMessage}`});
            }
        );
        updateStatus({messageLoading: false});
        setAppActionLoading(prev => ({...prev, [appName]: false}));
    };

    const handleDeleteClick = (appName: string) => {
        setAppToDelete(appName);
        setConfirmDeleteDialogOpen(true);
    };

    const handleConfirmDelete = () => {
        if (appToDelete) handleDeleteApp(appToDelete);
        setAppToDelete(null);
        setConfirmDeleteDialogOpen(false);
    };

    const handleCancelDelete = () => {
        setAppToDelete(null);
        setConfirmDeleteDialogOpen(false);
    };

    const handleStopApp = async (appName: string) => {
        clearMessages();
        updateStatus({messageLoading: true});
        setAppActionLoading(prev => ({...prev, [appName]: true}));
        await invokeTauriCommandWrapper<void>("stop_app", {appName}, () => {},
            (errorMessage, rawError) => {
                console.error(`Failed to stop app ${appName}:`, rawError);
                updateStatus({error: `Stop app ${appName} failed: ${errorMessage}`});
            }
        );
        updateStatus({messageLoading: false});
        setAppActionLoading(prev => ({...prev, [appName]: false}));
    };

    const handleCancelAppOperation = async (appName: string) => {
        try {
            await invoke('cancel_app_operation', {appName});
        } catch (rawError) {
            const errorMessage = rawError instanceof Error ? rawError.message : String(rawError);
            console.error(`Failed to cancel operation for ${appName}:`, rawError);
            updateStatus({error: `Cancel operation for ${appName} failed: ${errorMessage}`});
            throw rawError;
        }
    };

    const handleVersionSelected = (appName: string, targetVersion: string, currentAppVersion: string | null) => {
        if (!targetVersion) {
            // Version cleared — hide inline log
            setInlineUpdateLogs(prev => {
                const next = {...prev};
                delete next[appName];
                return next;
            });
            return;
        }
        const actionType = getVersionActionType(targetVersion, currentAppVersion);
        setInlineUpdateLogs(prev => ({...prev, [appName]: {version: targetVersion, actionType, isConfirming: false}}));
    };

    const handleGitVersionChange = async (params: { appName: string, version: string, actionType: VersionActionType }) => {
        clearMessages();
        activeGitUpdateAppsRef.current.add(params.appName);
        setAppActionLoading(prev => ({...prev, [params.appName]: true}));
        // Mark as confirming so the inline log shows a spinner
        setInlineUpdateLogs(prev => ({
            ...prev,
            [params.appName]: {...(prev[params.appName] ?? {version: params.version, actionType: params.actionType}), isConfirming: true, completed: false, failed: false}
        }));
        setStartingAppName(params.appName);
        beginConsoleSession(params.appName, `Initiating ${params.actionType} for '${params.appName}' to version '${params.version}'...`);
        setInlineConsoles(prev => ({...prev, [params.appName]: 'git-update'}));

        const requirementsFile = app?.profiles?.find(p => p.name === app.current_profile)?.requirements || "requirements.txt";

        await invokeTauriCommandWrapper<void>("update_to_version", {appName: params.appName, version: params.version, requirements: requirementsFile}, () => {},
            (errorMessage, rawError) => {
                console.error(`Failed to invoke ${params.actionType.toLowerCase()}:`, rawError);
                const operationError = `Upgrade failed: ${errorMessage}`;
                completedAppsRef.current.delete(params.appName);
                setInlineUpdateLogs(prev => {
                    const entry = prev[params.appName];
                    if (!entry) return prev;
                    return {...prev, [params.appName]: {...entry, isConfirming: false, completed: false, failed: true}};
                });
                addConsoleLog({message: operationError, app_name: params.appName, error: true, finished: true});
            }
        );
    };

    const handleMirrorVersionChange = async (params: {appName: string; version: string; actionType: VersionActionType}) => {
        if (!await ensureMirrorCdk()) return;
        setInlineUpdateLogs(prev => ({...prev, [params.appName]: {
            version: params.version, actionType: params.actionType, isConfirming: true,
        }}));
        beginMirrorOperation(params.appName);
        await invokeTauriCommandWrapper<void>('update_to_version', {
            appName: params.appName, version: params.version,
        }, () => {}, errorMessage => {
            setInlineUpdateLogs(prev => ({...prev, [params.appName]: {
                version: params.version, actionType: params.actionType, isConfirming: false, failed: true,
            }}));
            finishMirrorInvocationError(params.appName, errorMessage);
        });
    };

    const handleOpenGitInstallConsole = (appName: string) => {
        clearMessages();
        setStartingAppName(appName);
        setConsoleLogs(previous => previous[appName]?.length ? previous : {
            ...previous,
            [appName]: [{
                app_name: appName,
                message: app?.update_error ?? t('Installing App: {{appName}}', {appName}),
                error: app?.update_state === 'failed',
                finished: app?.update_state === 'failed',
            }],
        });
        setCurrentPage('installConsole');
    };

    const handleOpenRunningAppConsole = (appName: string) => {
        clearMessages();
        setStartingAppName(appName);
        const consoleTitle = `Console for running app: ${appName}`;
        setConsoleLogs(previous => previous[appName]?.length
            ? previous
            : {...previous, [appName]: [{message: consoleTitle, app_name: appName}]});
        setIsRunningAppConsoleOpen(true);
        setCurrentPage('runningAppConsole');
    };

    const handleOpenGitUpdateConsole = (appName: string) => {
        const entry = inlineUpdateLogs[appName];
        const version = app?.update_target_version ?? entry?.version;
        if (!version) return;
        const actionType = entry?.actionType ?? 'Upgrade';
        setStartingAppName(appName);
        setConsoleLogs(previous => previous[appName]?.length
            ? previous
            : {
                ...previous,
                [appName]: [{
                    message: app?.update_error ?? `${actionType} to ${version}`,
                    app_name: appName,
                    error: app?.update_state === 'failed',
                    finished: app?.update_state === 'failed',
                }],
            });
        setInlineConsoles(prev => ({...prev, [appName]: 'git-update'}));
    };

    const handleOpenMirrorConsole = (appName: string) => {
        setInlineConsoles(prev => ({...prev, [appName]: 'mirror'}));
    };

    const handleCloseMirrorConsole = async (appName: string) => {
        if (!activeMirrorAppsRef.current.has(appName) && app?.update_state !== 'updating') {
            setInlineUpdateLogs(prev => {
                const next = {...prev};
                delete next[appName];
                return next;
            });
            setSelectedTargetVersions(prev => ({...prev, [appName]: ''}));
        }
        await handleCloseInlineConsole(appName);
    };

    const handleCloseInlineConsole = async (appName: string) => {
        setInlineConsoles(prev => {
            const next = {...prev};
            delete next[appName];
            return next;
        });
        setAppActionLoading(prev => ({...prev, [appName]: false}));
        if (startingAppName === appName) {
            setStartingAppName(null);
            setIsStartAppProcessRunning(false);
        }
        await invokeTauriCommandWrapper<App>("load_app", undefined, () => {},
            (errorMessage, rawError) => {
                console.error("Failed to reload app:", rawError);
                updateStatus({error: `Failed to reload app: ${errorMessage}`});
            }
        );
    };

    const resetConsoleStates = () => {
        if (!gitInstallAppsRef.current.size) setIsGitInstallProcessRunning(false);
        setIsStartAppProcessRunning(false);
        setIsRunningAppConsoleOpen(false);
        setIsProfileChangeProcessRunning(false);
    }

    const handleBackFromConsole = async () => {
        setCurrentPage('list');
        resetConsoleStates();
        clearMessages();
        updateStatus({messageLoading: false});
        if (startingAppName) setAppActionLoading(prev => ({...prev, [startingAppName]: false}));
        setStartingAppName(null);
        setProfileChangeData(null);

        updateStatus({loading: true, info: t("Refreshing app...")});
        await invokeTauriCommandWrapper<App>("load_app", undefined,
            () => {
                updateStatus({loading: false, info: t("App Refreshed.")});
            },
            (errorMessage, rawError) => {
                console.error("Failed to reload app:", rawError);
                updateStatus({error: `Failed to reload app: ${errorMessage}`, loading: false});
            }
        );
    };

    const handleNavigateToChangeProfilePage = (appToChange: App) => {
        clearMessages();
        setAppForProfileChange(appToChange);
        const initialProfile = appToChange.profiles?.some(p => p.name === appToChange.current_profile)
            ? appToChange.current_profile
            : appToChange.profiles?.[0]?.name || "";
        setSelectedNewProfileName(initialProfile);
        setCurrentPage('changeProfile');
    };

    const handleConfirmProfileChange = async (appName: string, newProfileName: string) => {
        clearMessages();
        setAppActionLoading(prev => ({...prev, [appName]: true}));
        setStartingAppName(appName);
        setProfileChangeData({appName, newProfile: newProfileName});
        beginConsoleSession(appName, `Initiating profile change for '${appName}' to '${newProfileName}'...`);
        setIsProfileChangeProcessRunning(true);
        setCurrentPage('profileChangeConsole');

        await invokeTauriCommandWrapper<void>("setup_app", {appName, profileName: newProfileName}, () => {},
            (errorMessage, rawError) => {
                console.error(`Failed to invoke setup_app for profile change:`, rawError);
                addConsoleLog({message: `ERROR: Failed to change profile: ${errorMessage}`, app_name: appName, error: true, finished: true});
                setIsProfileChangeProcessRunning(false);
            }
        );
    };

    useEffect(() => {
        const shouldShow = (status.info || status.error) && !status.messageLoading &&
            ['list', 'settings', 'changeProfile'].includes(currentPage);
        if (shouldShow) {
            setSnackbarMessage(status.error || status.info || "");
            setSnackbarSeverity(status.error ? "error" : "info");
            setSnackbarOpen(true);
            const timerId = setTimeout(() => updateStatus({error: null, info: null}), status.error ? 8000 : 5000);
            return () => clearTimeout(timerId);
        }
        if (!status.info && !status.error) setSnackbarOpen(false);
    }, [status.info, status.error, status.messageLoading, updateStatus, currentPage]);

    const handleCheckForUpdates = async (appName: string) => {
        clearMessages();
        setAppActionLoading(prev => ({...prev, [appName]: true}));
        setCheckingUpdateForApp(appName);
        await invokeTauriCommandWrapper<void>("load_app", undefined,
            () => updateStatus({info: t("App Refreshed.")}),
            (errorMessage, rawError) => {
                console.error("Failed to check for updates:", rawError);
                updateStatus({error: `Failed to check for updates: ${errorMessage}`});
            }
        );
        setAppActionLoading(prev => ({...prev, [appName]: false}));
        setCheckingUpdateForApp(null);
    };

    const handleAddDefenderExclusion = async (appName: string) => {
        clearMessages();
        setAppActionLoading(prev => ({...prev, [appName]: true}));
        setAddingDefenderExclusionForApp(appName);
        await invokeTauriCommandWrapper<void>("add_defender_exclusion", {appName},
            () => {
                updateStatus({info: t('defenderExclusionAdded', {appName})});
                setHiddenDefenderButtons(prev => new Set(prev).add(appName));
            },
            (errorMessage, rawError) => {
                console.error(`Failed to add defender exclusion for ${appName}:`, rawError);
                updateStatus({error: t('failedToAddExclusion', {errorMessage})});
            }
        );
        setAddingDefenderExclusionForApp(null);
        setAppActionLoading(prev => ({...prev, [appName]: false}));
    };

    const isInlineConsoleVisible = currentPage === 'list' && !!app && !!inlineConsoles[app.name];
    let pageContent;

    if (currentPage === 'installConsole' && startingAppName) {
        pageContent = <GitInstallConsole
            appName={startingAppName}
            logs={consoleLogs[startingAppName] ?? []}
            onBack={handleBackFromConsole}
            onCancel={() => handleCancelAppOperation(startingAppName)}
            isProcessing={isGitInstallProcessRunning}
        />;
    } else if (currentPage === 'runningAppConsole' && startingAppName) {
        pageContent = <ConsolePage
            title={t('Console: {{appName}}', {appName: startingAppName})}
            appName={startingAppName}
            logs={consoleLogs[startingAppName] ?? []}
            onBack={handleBackFromConsole}
            isProcessing={isRunningAppConsoleOpen}
        />;
    } else if (currentPage === 'profileChangeConsole' && profileChangeData && startingAppName) {
        pageContent = <ConsolePage title={t("Changing Profile: {{appName}} to '{{newProfile}}'", { appName: profileChangeData.appName, newProfile: profileChangeData.newProfile })} appName={startingAppName} logs={consoleLogs[startingAppName] ?? []} onBack={handleBackFromConsole} onCancel={() => handleCancelAppOperation(startingAppName)} isProcessing={isProfileChangeProcessRunning}/>;
    } else if (currentPage === 'settings') {
        pageContent = <SettingsPage app={app} currentTheme={themeMode} onChangeTheme={setThemeMode} onBack={() => setCurrentPage('list')} updateStatus={updateStatus} clearMessages={clearMessages} />;
    } else if (currentPage === 'profileChooser' && profileChoiceApp) {
        pageContent = (
            <Container maxWidth="sm" sx={{py: 4}}>
                <Typography variant="h5" gutterBottom>{t('Choose Profile for {{appName}}', {appName: profileChoiceApp.name})}</Typography>
                {profileChoiceApp.profiles?.length > 0 ? (
                    <>
                        <FormControl fullWidth sx={{my: 2}}>
                            <InputLabel id="profile-select-label">{t('Profile')}</InputLabel>
                            <Select labelId="profile-select-label" value={selectedProfileForInstall} label={t('Profile')} onChange={(e) => setSelectedProfileForInstall(e.target.value)}>
                                {profileChoiceApp.profiles.map(p => <MenuItem key={p.name} value={p.name}>{p.name}</MenuItem>)}
                            </Select>
                        </FormControl>
                        <Stack direction="row" spacing={2} sx={{mt: 3, justifyContent: 'flex-end'}}>
                            <Button variant="outlined" onClick={() => setCurrentPage('list')}>{t('Cancel')}</Button>
                            <Button variant="contained" onClick={() => handleGitInstallWithProfile(profileChoiceApp.name, selectedProfileForInstall)} disabled={!selectedProfileForInstall || appActionLoading[profileChoiceApp.name]}>
                                {appActionLoading[profileChoiceApp.name] ? t("Starting Install...") : t("Confirm & Install")}
                            </Button>
                        </Stack>
                    </>
                ) : (
                    <>
                        <Typography sx={{my: 2}}>{t("No profiles available.")}</Typography>
                        <Button variant="outlined" onClick={() => setCurrentPage('list')}>{t('Back')}</Button>
                    </>
                )}
            </Container>
        );
    } else if (currentPage === 'changeProfile' && appForProfileChange) {
        pageContent = (
            <Container maxWidth="sm" sx={{py: 4}}>
                <Typography variant="h5" gutterBottom>{t('Change Profile for {{appName}}', {appName: appForProfileChange.name})}</Typography>
                <Typography variant="subtitle1" gutterBottom>{t('Current Profile: {{profile}}', {profile: appForProfileChange.current_profile})}</Typography>
                {appForProfileChange.profiles?.length > 0 ? (
                    <>
                        <FormControl fullWidth sx={{my: 2}}>
                            <InputLabel id="change-profile-select-label">{t('New Profile')}</InputLabel>
                            <Select labelId="change-profile-select-label" value={selectedNewProfileName} label={t('New Profile')} onChange={(e) => setSelectedNewProfileName(e.target.value)}>
                                {appForProfileChange.profiles.map(p => <MenuItem key={p.name} value={p.name} disabled={p.name === appForProfileChange.current_profile}>{p.name}{p.name === appForProfileChange.current_profile && t(" (Current)")}</MenuItem>)}
                            </Select>
                        </FormControl>
                        <Stack direction="row" spacing={2} sx={{mt: 3, justifyContent: 'flex-end'}}>
                            <Button variant="outlined" onClick={() => setCurrentPage('list')}>{t('Cancel')}</Button>
                            <Button variant="contained" onClick={() => handleConfirmProfileChange(appForProfileChange.name, selectedNewProfileName)} disabled={!selectedNewProfileName || selectedNewProfileName === appForProfileChange.current_profile || appActionLoading[appForProfileChange.name]}>
                                {appActionLoading[appForProfileChange.name] ? t("Initiating...") : t("Change Profile")}
                            </Button>
                        </Stack>
                    </>
                ) : <Typography sx={{my: 2}}>{t("No profiles available.")}</Typography>}
            </Container>
        );
    } else {
        pageContent = (
            <Container maxWidth="lg" sx={{
                py: isInlineConsoleVisible ? 2 : {xs: 2, md: 4},
                height: isInlineConsoleVisible ? '100vh' : 'auto',
                boxSizing: 'border-box',
                overflow: isInlineConsoleVisible ? 'hidden' : 'visible',
            }}>
                <Snackbar open={snackbarOpen} autoHideDuration={6000} onClose={() => setSnackbarOpen(false)} anchorOrigin={{vertical: 'bottom', horizontal: 'center'}}>
                    <Alert onClose={() => setSnackbarOpen(false)} severity={snackbarSeverity} sx={{width: '100%'}}>{snackbarMessage}</Alert>
                </Snackbar>
                {status.loading && !app && (
                    <Card variant="outlined" sx={{borderColor: 'divider', boxShadow: 'none'}}>
                        <CardContent sx={{display: 'flex', justifyContent: 'center', alignItems: 'center', minHeight: 220}}>
                            <CircularProgress size={24}/><Typography sx={{ml: 1.5}} color="text.secondary">{t('Loading app...')}</Typography>
                        </CardContent>
                    </Card>
                )}
                {!status.loading && !app && (
                    <Card variant="outlined" sx={{borderColor: 'divider', boxShadow: 'none'}}>
                        <CardContent sx={{py: 8, textAlign: 'center'}}><Typography color="text.secondary">{t('App configuration could not be loaded.')}</Typography></CardContent>
                    </Card>
                )}
                {app && (
                    (() => {
                            const isGitInstalling = app.update_source === 'git' && !app.installed
                                && (app.running || isGitInstallProcessRunning || app.update_state === 'updating');
                            const hasMirrorOperation = app.update_source === 'mirrorchyan'
                                && (isMirrorProcessRunning || app.update_state !== 'idle');
                            const hasGitInstallFailure = app.update_source === 'git' && !app.installed
                                && app.update_state === 'failed';
                            const isThisAppLoading = appActionLoading[app.name] || false;
                            const updateBlocksActions = app.update_state !== 'idle';
                            const persistedActionType = getVersionActionType(
                                app.update_target_version ?? '',
                                app.current_version,
                                'Upgrade',
                            );
                            const disableRowActions = currentPage !== 'list' || status.messageLoading || isThisAppLoading || updateBlocksActions;
                            const disableUpdateControls = currentPage !== 'list' || status.messageLoading || isThisAppLoading
                                || isMirrorProcessRunning || isGitInstallProcessRunning || app.update_state === 'updating';
                            const inlineConsoleKind = inlineConsoles[app.name];
                            const inlineUpdateEntry = inlineUpdateLogs[app.name];
                            const inlineUpdateAction = inlineUpdateEntry?.actionType ?? persistedActionType;
                        return (
                                <Card
                                    key={app.name}
                                    variant="outlined"
                                    sx={{
                                        width: '100%',
                                        borderColor: app.running ? 'success.main' : 'divider',
                                        bgcolor: 'background.paper',
                                        overflow: inlineConsoleKind ? 'hidden' : 'visible',
                                        height: inlineConsoleKind ? '100%' : 'auto',
                                    }}
                                >
                                    <CardContent sx={{
                                        p: {xs: 2, sm: 3},
                                        '&:last-child': {pb: {xs: 2, sm: 3}},
                                        height: inlineConsoleKind ? '100%' : 'auto',
                                        boxSizing: 'border-box',
                                        display: inlineConsoleKind ? 'flex' : 'block',
                                        flexDirection: inlineConsoleKind ? 'column' : undefined,
                                    }}>
                                        <Stack direction={{xs: 'column', sm: 'row'}} spacing={2} sx={{justifyContent: 'space-between', alignItems: {xs: 'stretch', sm: 'flex-start'}}}>
                                            <Stack direction="row" spacing={1.5} sx={{minWidth: 0, alignItems: 'center'}}>
                                                <AppIcon appName={app.name} iconPath={app.icon}/>
                                                <Box sx={{minWidth: 0}}>
                                                    <Stack direction="row" spacing={1.5} useFlexGap sx={{alignItems: 'center', flexWrap: 'wrap'}}>
                                                        {app.website?.trim() ? (
                                                            <Link
                                                                component="button"
                                                                type="button"
                                                                variant="h6"
                                                                underline="hover"
                                                                onClick={handleOpenWebsite}
                                                                sx={{fontWeight: 500, maxWidth: '100%', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', textAlign: 'left'}}
                                                            >
                                                                {app.name}
                                                            </Link>
                                                        ) : (
                                                            <Typography variant="h6" noWrap>{app.name}</Typography>
                                                        )}
                                                        {app.installed && !app.running && (
                                                            <FormControlLabel
                                                                sx={{m: 0}}
                                                                control={(
                                                                    <Switch
                                                                        size="small"
                                                                        checked={app.auto_start}
                                                                        disabled={disableRowActions}
                                                                        onChange={(event) => handleAppPreferenceChange(app.name, null, event.target.checked)}
                                                                    />
                                                                )}
                                                                label={<Typography variant="body2" sx={{fontWeight: 600}}>{t('Auto Start')}</Typography>}
                                                            />
                                                        )}
                                                    </Stack>
                                                    <Stack direction="row" spacing={0.75} useFlexGap sx={{mt: 0.75, flexWrap: 'wrap'}}>
                                                        <Chip
                                                            size="small"
                                                            color={isGitInstalling ? 'info' : app.running ? 'success' : 'default'}
                                                            variant={app.running || isGitInstalling ? 'filled' : 'outlined'}
                                                            label={app.running && app.installed ? t('(Running)') : isGitInstalling ? t('(Installing...)') : app.installed ? t('Installed') : t('(Not Installed)')}
                                                            sx={{fontWeight: 650}}
                                                        />
                                                        {app.installed && app.current_version && (
                                                            <Chip size="small" variant="outlined" label={app.current_version}/>
                                                        )}
                                                        {app.installed && app.current_profile && (
                                                            <Chip size="small" variant="outlined" label={app.current_profile}/>
                                                        )}
                                                        {app.update_state === 'updating' && !isGitInstalling && (
                                                            <Chip size="small" color="info" icon={<CircularProgress size={14}/>} label={t(app.update_source === 'mirrorchyan' ? '(Installing...)' : getVersionActionProgressKey(persistedActionType))}/>
                                                        )}
                                                        {app.update_state === 'failed' && (
                                                            <Chip size="small" color="error" label={t(app.update_source === 'mirrorchyan' || hasGitInstallFailure ? 'Installation failed.' : `${persistedActionType} failed`)}/>
                                                        )}
                                                    </Stack>
                                                    {!app.installed && app.installation && !isGitInstalling && !hasMirrorOperation && (
                                                        <Typography variant="body2" color="text.secondary" sx={{mt: 1}}>
                                                            {t('selectedSourceNotInstalled')}
                                                        </Typography>
                                                    )}
                                                </Box>
                                            </Stack>
                                            <Stack
                                                direction="row"
                                                spacing={1}
                                                useFlexGap
                                                sx={{flexShrink: 0, flexWrap: 'wrap', justifyContent: {xs: 'flex-start', sm: 'flex-end'}}}
                                            >
                                                {app.installed ? (
                                                    app.running ? (
                                                        <>
                                                            <Button variant="contained" color="warning" startIcon={isThisAppLoading ? <CircularProgress size={16}/> : <StopCircle/>} onClick={() => handleStopApp(app.name)} disabled={disableRowActions}>{t("Stop App")}</Button>
                                                            <Button variant="outlined" color="info" size="small" startIcon={<OpenInNew/>} onClick={() => handleOpenRunningAppConsole(app.name)} disabled={disableRowActions}>{t('Console')}</Button>
                                                        </>
                                                    ) : (
                                                        <>
                                                            <Button variant="contained" color="success" startIcon={isThisAppLoading ? <CircularProgress size={16}/> : <PlayArrow/>} onClick={() => handleStartApp(app.name)} disabled={disableRowActions || !app.current_version}>{t("Start App")}</Button>
                                                        </>
                                                    )
                                                ) : isGitInstalling ? (
                                                    <Button variant="outlined" color="info" size="small" startIcon={<OpenInNew/>} onClick={() => handleOpenGitInstallConsole(app.name)}>{t('Console')}</Button>
                                                ) : (
                                                    <Button variant="contained" color="primary" startIcon={isThisAppLoading ? <CircularProgress size={16}/> : <Build/>} endIcon={<KeyboardArrowRight/>} onClick={() => handleInstallClick(app)} disabled={disableUpdateControls}>{t("Install")}</Button>
                                                )}
                                                {hasMirrorOperation && (
                                                    <Button variant="outlined" color="info" size="small" startIcon={<OpenInNew/>} onClick={() => handleOpenMirrorConsole(app.name)}>{t('Console')}</Button>
                                                )}
                                                {hasGitInstallFailure && (
                                                    <Button variant="outlined" color="info" size="small" startIcon={<OpenInNew/>} onClick={() => handleOpenGitInstallConsole(app.name)}>{t('Console')}</Button>
                                                )}
                                                {app.show_add_defender && !hiddenDefenderButtons.has(app.name) && <Button variant="outlined" color="secondary" size="small" startIcon={isThisAppLoading && addingDefenderExclusionForApp === app.name ? <CircularProgress size={16}/> : <Build/>} onClick={() => handleAddDefenderExclusion(app.name)} disabled={disableRowActions}>{t("Add Defender Exclusion")}</Button>}
                                                {app.installed && !app.running && app.profiles?.length > 1 && <Button variant="outlined" color="secondary" size="small" startIcon={isThisAppLoading ? <CircularProgress size={16}/> : <Cached/>} onClick={() => handleNavigateToChangeProfilePage(app)} disabled={disableRowActions}>{t("Change Profile")}</Button>}
                                                {app.installed && (
                                                    <Tooltip title={t('Delete')}>
                                                        <span>
                                                            <IconButton color="error" onClick={() => handleDeleteClick(app.name)} disabled={disableRowActions || app.running} sx={{border: 1, borderColor: 'divider', borderRadius: 2}}>
                                                                {isThisAppLoading ? <CircularProgress size={18}/> : <Delete fontSize="small"/>}
                                                            </IconButton>
                                                        </span>
                                                    </Tooltip>
                                                )}
                                                <Tooltip title={t('Settings')}>
                                                    <IconButton
                                                        onClick={() => setCurrentPage('settings')}
                                                        color="inherit"
                                                        sx={{border: 1, borderColor: 'divider', borderRadius: 2}}
                                                    >
                                                        <SettingsIcon fontSize="small"/>
                                                    </IconButton>
                                                </Tooltip>
                                            </Stack>
                                        </Stack>
                                        {((app.installed && !app.running) || !!inlineConsoleKind) && (
                                            <Box
                                                sx={{
                                                    mt: 2.5,
                                                    p: {xs: 1.5, sm: 2},
                                                    borderRadius: 3,
                                                    bgcolor: theme => alpha(theme.palette.primary.main, theme.palette.mode === 'dark' ? 0.075 : 0.045),
                                                    flex: inlineConsoleKind ? '1 1 auto' : undefined,
                                                    minHeight: inlineConsoleKind ? 0 : undefined,
                                                    display: inlineConsoleKind ? 'flex' : 'block',
                                                    flexDirection: inlineConsoleKind ? 'column' : undefined,
                                                }}
                                            >
                                                {app.installed && !app.running && (
                                                    <Stack direction={{xs: 'column', sm: 'row'}} spacing={1} sx={{alignItems: {xs: 'stretch', sm: 'center'}}}>
                                                        <FormControl size="small" sx={{minWidth: {xs: '100%', sm: 220}}} disabled={disableUpdateControls}>
                                                            <InputLabel>{t('Update Method')}</InputLabel>
                                                            <Select
                                                                value={app.update_method}
                                                                label={t('Update Method')}
                                                                onChange={(event) => handleAppPreferenceChange(app.name, event.target.value, null)}
                                                            >
                                                                {UPDATE_METHOD_OPTIONS.map(option => (
                                                                    <MenuItem key={option} value={option}>{t(option)}</MenuItem>
                                                                ))}
                                                            </Select>
                                                        </FormControl>
                                                        {app.available_versions.filter(v => v !== app.current_version || (app.update_source === 'mirrorchyan' && app.update_state === 'failed')).length > 0 ? (
                                                            <FormControl size="small" sx={{minWidth: {xs: '100%', sm: 220}}} disabled={disableUpdateControls}>
                                                                <InputLabel>{t('Change version...')}</InputLabel>
                                                                <Select
                                                                    value={selectedTargetVersions[app.name] || ''}
                                                                    label={t('Change version...')}
                                                                    renderValue={(selected) => {
                                                                        const selectedVersion = String(selected);
                                                                        return selectedVersion ? `${selectedVersion} ${t(getVersionChannelLabelKey(selectedVersion))}` : t('Change version...');
                                                                    }}
                                                                    onChange={(e) => {
                                                                        const newVer = e.target.value;
                                                                        setSelectedTargetVersions(p => ({...p, [app.name]: newVer}));
                                                                        handleVersionSelected(app.name, newVer, app.current_version);
                                                                    }}
                                                                >
                                                                    <MenuItem value=""><em>{t('Change version...')}</em></MenuItem>
                                                                    {app.available_versions.filter(v => v !== app.current_version || (app.update_source === 'mirrorchyan' && app.update_state === 'failed')).map(v => <MenuItem key={v} value={v}>{v} {t(getVersionChannelLabelKey(v))}{compareVersions(v, app.current_version!) < 0 ? ` ${t('(Downgrade)')}` : ` ${t('(Upgrade)')}`}</MenuItem>)}
                                                                </Select>
                                                            </FormControl>
                                                        ) : <Typography variant="caption">{t("No other versions found.")}</Typography>}
                                                        <Tooltip title={t("Check for updates")}><span><IconButton onClick={() => handleCheckForUpdates(app.name)} disabled={app.update_source === 'mirrorchyan' ? disableUpdateControls : disableRowActions} sx={{bgcolor: 'background.paper', border: 1, borderColor: 'divider', borderRadius: 2}}>{isThisAppLoading && checkingUpdateForApp === app.name ? <CircularProgress size={20}/> : <Cached fontSize="small"/>}</IconButton></span></Tooltip>
                                                    </Stack>
                                                )}
                                                {inlineConsoleKind === 'mirror' ? (
                                                    <MirrorOperationConsole
                                                        appName={app.name}
                                                        logs={consoleLogs[app.name] ?? []}
                                                        onBack={() => handleCloseMirrorConsole(app.name)}
                                                        onCancel={() => handleCancelAppOperation(app.name)}
                                                        isProcessing={isMirrorProcessRunning || app.update_state === 'updating'}
                                                        failed={app.update_state === 'failed'}
                                                        progress={mirrorUpdateProgress[app.name]}
                                                    />
                                                ) : inlineConsoleKind === 'git-update' ? (
                                                    <GitUpdateConsole
                                                        appName={app.name}
                                                        actionType={inlineUpdateAction}
                                                        logs={consoleLogs[app.name] ?? []}
                                                        onBack={() => handleCloseInlineConsole(app.name)}
                                                        onCancel={() => handleCancelAppOperation(app.name)}
                                                        isProcessing={app.update_state === 'updating' || !!inlineUpdateEntry?.isConfirming}
                                                    />
                                                ) : inlineConsoleKind === 'start' ? (
                                                    <ConsolePage
                                                        inline
                                                        title={t('Starting App: {{appName}}', {appName: app.name})}
                                                        appName={app.name}
                                                        logs={consoleLogs[app.name] ?? []}
                                                        onBack={() => handleCloseInlineConsole(app.name)}
                                                        isProcessing={isStartAppProcessRunning && startingAppName === app.name}
                                                    />
                                                ) : inlineUpdateEntry && (
                                                    <UpdateLogPage
                                                        appName={app.name}
                                                        version={inlineUpdateEntry.version}
                                                        actionType={inlineUpdateEntry.actionType}
                                                        isConfirming={inlineUpdateEntry.isConfirming}
                                                        completed={inlineUpdateEntry.completed}
                                                        failed={inlineUpdateEntry.failed}
                                                        website={app.website}
                                                        onConfirm={app.update_source === 'mirrorchyan' ? handleMirrorVersionChange : handleGitVersionChange}
                                                        onOpenConsole={() => app.update_source === 'mirrorchyan' ? handleOpenMirrorConsole(app.name) : handleOpenGitUpdateConsole(app.name)}
                                                        onCancel={() => {
                                                            completedAppsRef.current.delete(app.name);
                                                            setSelectedTargetVersions(p => ({...p, [app.name]: ''}));
                                                            setInlineUpdateLogs(prev => {
                                                                const next = {...prev};
                                                                delete next[app.name];
                                                                return next;
                                                            });
                                                        }}
                                                    />
                                                )}
                                            </Box>
                                        )}
                                    </CardContent>
                                </Card>
                        );
                    })()
                )}
                <Dialog open={isConfirmDeleteDialogOpen} onClose={handleCancelDelete}>
                    <DialogTitle>{t('Confirm Deletion')}</DialogTitle>
                    <DialogContent><DialogContentText>{appToDelete && t('Are you sure you want to delete {{appName}}?', {appName: appToDelete})}</DialogContentText></DialogContent>
                    <DialogActions><Button onClick={handleCancelDelete}>{t('Cancel')}</Button><Button onClick={handleConfirmDelete} color="error" autoFocus>{t('Delete')}</Button></DialogActions>
                </Dialog>
            </Container>
        );
    }
    return (
        <ThemeProvider theme={muiTheme}>
            <CssBaseline/>
            <Box sx={{display: 'flex', flexDirection: 'column', minHeight: '100vh'}}>
                <Box component="main" sx={{flex: '1 1 auto'}}>{pageContent}</Box>
                {currentPage === 'list' && !isInlineConsoleVisible && (
                    <Box component="footer" sx={{py: 2, textAlign: 'center'}}>
                        <Typography variant="body2" color="text.secondary">
                            <Link href="https://github.com/ok-oldking/pyappify" target="_blank" rel="noopener noreferrer">{t('appMadeWith', {name: `PyAppify ${appVersion}`})}</Link>
                        </Typography>
                    </Box>
                )}
            </Box>
        </ThemeProvider>
    );
}
export default App;
