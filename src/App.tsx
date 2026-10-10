import './i18n';
import {useCallback, useEffect, useMemo, useState} from 'react';
import {getVersion} from '@tauri-apps/api/app';
import {getCurrentWindow} from '@tauri-apps/api/window';
import {useTranslation} from 'react-i18next';
import {Alert, Box, Button, Card, CardContent, CircularProgress, Container, Dialog, DialogActions, DialogContent, DialogContentText, DialogTitle, FormControl, InputLabel, Link, MenuItem, Select, Snackbar, Stack, Typography} from '@mui/material';
import {createTheme, ThemeProvider} from '@mui/material/styles';
import CssBaseline from '@mui/material/CssBaseline';
import useMediaQuery from '@mui/material/useMediaQuery';
import AppCard from './features/app/AppCard';
import TaskConsole from './features/operations/TaskConsole';
import {isTaskActive, taskForApp} from './features/operations/model';
import {useLauncherController} from './features/operations/useLauncherController';
import ConsolePage from './ConsolePage';
import SettingsPage from './SettingsPage';
import type {App as LauncherApp, ThemeModeSetting} from './types';

function App() {
    const {t} = useTranslation();
    const [navigation, setNavigation] = useState<{page: string; console: 'task' | 'start' | null; visible: boolean}>({page: 'list', console: null, visible: false});
    const currentPage = navigation.page;
    const inlineConsole = navigation.visible ? navigation.console : null;
    const setCurrentPage = (page: string) => setNavigation(previous => ({...previous, page}));
    const setInlineConsole = (console: 'task' | 'start' | null) => setNavigation(previous => ({...previous, console: console ?? previous.console, visible: console !== null}));
    const [profileChoiceApp, setProfileChoiceApp] = useState<LauncherApp | null>(null);
    const [selectedProfileForInstall, setSelectedProfileForInstall] = useState('');
    const [appForProfileChange, setAppForProfileChange] = useState<LauncherApp | null>(null);
    const [selectedNewProfileName, setSelectedNewProfileName] = useState('');
    const [isConfirmDeleteDialogOpen, setConfirmDeleteDialogOpen] = useState(false);
    const [appVersion, setAppVersion] = useState('');
    const [snackbarOpen, setSnackbarOpen] = useState(false);
    const [snackbarMessage, setSnackbarMessage] = useState('');
    const [snackbarSeverity, setSnackbarSeverity] = useState<'info' | 'error'>('info');
    const [themeMode, setThemeMode] = useState<ThemeModeSetting>(() => {
        const saved = localStorage.getItem('appThemeMode');
        return saved === 'light' || saved === 'dark' ? saved : 'system';
    });
    const chooseProfile = useCallback((app: LauncherApp) => {
        setProfileChoiceApp(app);
        setSelectedProfileForInstall(app.profiles.some(profile => profile.name === app.current_profile) ? app.current_profile : app.profiles[0]?.name || 'default');
        setCurrentPage('profileChooser');
    }, []);
    const state = useLauncherController(t, chooseProfile);
    const {app, controller, status} = state;
    const task = taskForApp(state.task, app);
    const busy = isTaskActive(task) || !!state.request;
    const {updateStatus, clearMessages} = controller;
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


    useEffect(() => {void getVersion().then(setAppVersion);}, []);
    useEffect(() => {localStorage.setItem('appThemeMode', themeMode);}, [themeMode]);
    useEffect(() => {
        if (!app?.name) return;
        const title = t('appLauncherName', {appName: app.name});
        document.title = title;
        void getCurrentWindow().setTitle(title).catch(console.warn);
    }, [app?.name, t]);
    useEffect(() => {if (task && isTaskActive(task)) setInlineConsole('task');}, [task?.id]);
    useEffect(() => {
        const visible = (status.info || status.error) && !status.messageLoading && ['list', 'settings', 'changeProfile'].includes(currentPage);
        if (visible) {
            setSnackbarMessage(status.error || status.info || '');
            setSnackbarSeverity(status.error ? 'error' : 'info');
            setSnackbarOpen(true);
            const timer = setTimeout(() => updateStatus({error: null, info: null}), status.error ? 8000 : 5000);
            return () => clearTimeout(timer);
        }
        if (!status.info && !status.error) setSnackbarOpen(false);
    }, [status.info, status.error, status.messageLoading, currentPage, updateStatus]);
    const back = () => {setCurrentPage('list'); setInlineConsole(null);};
    const handleInstallWithProfile = async (_appName: string, profileName: string) => {
        if (!await controller.ensureSourceReady()) {setCurrentPage('settings'); return;}
        if (!app || busy || app.running) return;
        const inline = app.update_source === 'mirrorchyan';
        setCurrentPage(inline ? 'list' : 'installConsole');
        setInlineConsole(inline ? 'task' : null);
        void controller.install(profileName);
    };
    const install = () => {
        if (!app) return;
        if (app.update_source === 'git' && app.profiles.length > 1) chooseProfile(app);
        else void handleInstallWithProfile(app.name, app.current_profile || app.profiles[0]?.name || 'default');
    };
    const handleConfirmProfileChange = (_appName: string, profileName: string) => {
        if (busy || app?.running) return;
        setCurrentPage('profileChangeConsole');
        void controller.configure(profileName);
    };
    const confirmVersion = async ({version, notes}: {version: string; notes?: string}) => {
        if (!await controller.ensureSourceReady('update')) {setCurrentPage('settings'); return;}
        setInlineConsole('task');
        void controller.update(version, notes);
    };
    const openConsole = () => {
        if (!isTaskActive(task) && (app?.running || state.request === 'start' || navigation.console === 'start')) setCurrentPage('runningAppConsole');
        else if (task) {
            if (task.kind.endsWith('_configure')) setCurrentPage('profileChangeConsole');
            else if (task.kind === 'git_install') setCurrentPage('installConsole');
            else setInlineConsole('task');
        } else setCurrentPage('runningAppConsole');
    };
    const startApp = () => {setInlineConsole('start'); void controller.start();};
    const changeProfile = () => {
        if (!app) return;
        clearMessages(); setAppForProfileChange(app); setSelectedNewProfileName(app.current_profile); setCurrentPage('changeProfile');
    };
    const handleCancelDelete = () => setConfirmDeleteDialogOpen(false);
    const handleConfirmDelete = () => {setConfirmDeleteDialogOpen(false); setInlineConsole(null); void controller.delete();};
    const isInlineConsoleVisible = currentPage === 'list' && !!inlineConsole;
    let pageContent;
    if ((currentPage === 'installConsole' || currentPage === 'profileChangeConsole') && task) {
        pageContent = <TaskConsole task={task} onBack={back} onCancel={controller.cancel}/>;
    } else if (currentPage === 'runningAppConsole' && app) {
        pageContent = <ConsolePage title={t('Console: {{appName}}', {appName: app.name})} appName={app.name}
            logs={state.applicationLogs} outcome={state.applicationOutcome} onBack={back} isProcessing={app.running || state.request === 'start'}/>;
    } else if (currentPage === 'settings') {
        pageContent = <SettingsPage app={app} busy={busy} saving={state.request === 'settings'} runRequest={controller.setting} currentTheme={themeMode} onChangeTheme={setThemeMode}
            onBack={() => setCurrentPage('list')} updateStatus={updateStatus} clearMessages={clearMessages}/>;
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
                            <Button variant="contained" onClick={() => handleInstallWithProfile(profileChoiceApp.name, selectedProfileForInstall)} disabled={!selectedProfileForInstall || busy}>
                                {busy ? t("Starting Install...") : t("Confirm & Install")}
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
                            <Button variant="contained" onClick={() => handleConfirmProfileChange(appForProfileChange.name, selectedNewProfileName)} disabled={!selectedNewProfileName || selectedNewProfileName === appForProfileChange.current_profile || busy}>
                                {busy ? t("Initiating...") : t("Change Profile")}
                            </Button>
                        </Stack>
                    </>
                ) : <Typography sx={{my: 2}}>{t("No profiles available.")}</Typography>}
            </Container>
        );

    } else {
        pageContent = <Container maxWidth="lg" sx={{py: isInlineConsoleVisible ? 2 : {xs: 2, md: 4}, height: isInlineConsoleVisible ? '100vh' : 'auto', boxSizing: 'border-box', overflow: isInlineConsoleVisible ? 'hidden' : 'visible'}}>
            {status.loading && !app && <Card variant="outlined" sx={{borderColor: 'divider', boxShadow: 'none'}}><CardContent sx={{display: 'flex', justifyContent: 'center', alignItems: 'center', minHeight: 220}}><CircularProgress size={24}/><Typography sx={{ml: 1.5}} color="text.secondary">{t('Loading app...')}</Typography></CardContent></Card>}
            {!status.loading && !app && <Card variant="outlined" sx={{borderColor: 'divider', boxShadow: 'none'}}><CardContent sx={{py: 8, textAlign: 'center'}}><Typography color="text.secondary">{t('App configuration could not be loaded.')}</Typography></CardContent></Card>}
            {app && <AppCard state={state} controller={controller} inlineConsole={inlineConsole} onConsole={openConsole}
                onCloseConsole={() => setInlineConsole(null)} onInstall={install} onProfileChange={changeProfile}
                onDelete={() => setConfirmDeleteDialogOpen(true)} onSettings={() => setCurrentPage('settings')}
                startApp={startApp} confirmVersion={confirmVersion}/>}
            <Dialog open={isConfirmDeleteDialogOpen} onClose={handleCancelDelete}>
                <DialogTitle>{t('Confirm Deletion')}</DialogTitle>
                <DialogContent><DialogContentText>{app && t('Are you sure you want to delete {{appName}}?', {appName: app.name})}</DialogContentText></DialogContent>
                <DialogActions><Button onClick={handleCancelDelete}>{t('Cancel')}</Button><Button onClick={handleConfirmDelete} color="error" autoFocus>{t('Delete')}</Button></DialogActions>
            </Dialog>
        </Container>;
    }
    return <ThemeProvider theme={muiTheme}><CssBaseline/><Box sx={{display: 'flex', flexDirection: 'column', minHeight: '100vh'}}>
            <Snackbar open={snackbarOpen} autoHideDuration={6000} onClose={() => setSnackbarOpen(false)} anchorOrigin={{vertical: 'bottom', horizontal: 'center'}}>
                <Alert onClose={() => setSnackbarOpen(false)} severity={snackbarSeverity} sx={{width: '100%'}}>{snackbarMessage}</Alert>
            </Snackbar>
        <Box component="main" sx={{flex: '1 1 auto'}}>{pageContent}</Box>
        {currentPage === 'list' && !isInlineConsoleVisible && <Box component="footer" sx={{py: 2, textAlign: 'center'}}><Typography variant="body2" color="text.secondary"><Link href="https://github.com/ok-oldking/pyappify" target="_blank" rel="noopener noreferrer">{t('appMadeWith', {name: `PyAppify ${appVersion}`})}</Link></Typography></Box>}
    </Box></ThemeProvider>;
}
export default App;
