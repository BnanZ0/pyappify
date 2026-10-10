import {useTranslation} from 'react-i18next';
import {Box, Button, Card, CardContent, Chip, CircularProgress, FormControl, FormControlLabel, IconButton, InputLabel, Link, MenuItem, Select, Stack, Switch, Tooltip, Typography} from '@mui/material';
import {Build, Cached, Delete, KeyboardArrowRight, OpenInNew, PlayArrow, Settings as SettingsIcon, StopCircle} from '@mui/icons-material';
import {alpha} from '@mui/material/styles';
import {openUrl} from '@tauri-apps/plugin-opener';
import AppIcon from './AppIcon';
import {compareVersions, getVersionActionProgressKey, getVersionActionType, getVersionChannelLabelKey} from './versions';
import {sourceNeedsInstallation} from '../mirror/operationUi';
import {isTaskActive, taskForApp} from '../operations/model';
import TaskConsole from '../operations/TaskConsole';
import type {LauncherState, LauncherController} from '../operations/controller';
import ConsolePage from '../../ConsolePage';
import UpdateLogPage from '../../UpdateLogPage';

export default function AppCard({state, controller, inlineConsole, onConsole, onCloseConsole, onInstall, onProfileChange, onDelete, onSettings, startApp, confirmVersion}: {
    state: LauncherState; controller: LauncherController; inlineConsole: 'task' | 'start' | null;
    onConsole: () => void; onCloseConsole: () => void; onInstall: () => void; onProfileChange: () => void;
    onDelete: () => void; onSettings: () => void; startApp: () => void; confirmVersion: (params: {version: string; notes?: string}) => void;
}) {
    const {t} = useTranslation();
    const app = state.app!;
    const task = taskForApp(state.task, state.app);
    const taskActive = isTaskActive(task);
    const isEffectivelyInstalling = taskActive && !!task?.kind.endsWith('_install');
    const selectedInstalled = app.installed && !sourceNeedsInstallation(app);
    const selectedRunning = selectedInstalled && app.running && !taskActive;
    const isThisAppLoading = !!state.request || taskActive;
    const disableRowActions = isThisAppLoading || (selectedInstalled && app.update_state !== 'idle') || app.source_operation_state === 'updating';
    const disableUpdateControls = isThisAppLoading || app.update_state === 'updating' || app.source_operation_state === 'updating';
    const persistedActionType = task?.action ?? getVersionActionType(app.update_target_version ?? '', app.current_version, 'Upgrade');
    const inlineUpdateEntry = state.proposal;
    const UPDATE_METHOD_OPTIONS = ['MANUAL_UPDATE', 'AUTO_UPDATE', 'AUTO_UPDATE_PRE_RELEASE'] as const;
    const openWebsite = () => {if (app.website?.trim()) void openUrl(app.website.trim()).catch(console.warn);};
    return (
        <Card
            key={app.name}
            variant="outlined"
            sx={{
                width: '100%',
                borderColor: selectedRunning ? 'success.main' : 'divider',
                bgcolor: 'background.paper',
                overflow: inlineConsole ? 'hidden' : 'visible',
                height: inlineConsole ? '100%' : 'auto',
            }}
        >
            <CardContent sx={{
                p: {xs: 2, sm: 3},
                '&:last-child': {pb: {xs: 2, sm: 3}},
                height: inlineConsole ? '100%' : 'auto',
                boxSizing: 'border-box',
                display: inlineConsole ? 'flex' : 'block',
                flexDirection: inlineConsole ? 'column' : undefined,
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
                                        onClick={openWebsite}
                                        sx={{fontWeight: 500, maxWidth: '100%', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', textAlign: 'left'}}
                                    >
                                        {app.name}
                                    </Link>
                                ) : (
                                    <Typography variant="h6" noWrap>{app.name}</Typography>
                                )}
                                {selectedInstalled && !selectedRunning && (
                                    <FormControlLabel
                                        sx={{m: 0}}
                                        control={(
                                            <Switch
                                                size="small"
                                                checked={app.auto_start}
                                                disabled={disableRowActions}
                                                onChange={(event) => controller.preferences(null, event.target.checked)}
                                            />
                                        )}
                                        label={<Typography variant="body2" sx={{fontWeight: 600}}>{t('Auto Start')}</Typography>}
                                    />
                                )}
                            </Stack>
                            <Stack direction="row" spacing={0.75} useFlexGap sx={{mt: 0.75, flexWrap: 'wrap'}}>
                                <Chip
                                    size="small"
                                    color={selectedRunning ? 'success' : isEffectivelyInstalling ? 'info' : 'default'}
                                    variant={selectedRunning || isEffectivelyInstalling ? 'filled' : 'outlined'}
                                    label={selectedRunning && selectedInstalled ? t('(Running)') : isEffectivelyInstalling ? t('(Installing...)') : selectedInstalled ? t('Installed') : t('(Not Installed)')}
                                    sx={{fontWeight: 650}}
                                />
                                {selectedInstalled && app.current_version && (
                                    <Chip size="small" variant="outlined" label={app.current_version}/>
                                )}
                                {selectedInstalled && app.current_profile && (
                                    <Chip size="small" variant="outlined" label={app.current_profile}/>
                                )}
                                {taskActive && !!task?.kind.endsWith('_update') && (
                                    <Chip size="small" color="info" icon={<CircularProgress size={14}/>} label={t(getVersionActionProgressKey(persistedActionType))}/>
                                )}
                                {!taskActive && task?.status === 'failed' && task.kind.endsWith('_update') && (
                                    <Chip size="small" color="error" label={t(`${persistedActionType} failed`)}/>
                                )}
                            </Stack>
                            {task?.status === 'failed' && !task.kind.endsWith('_update') && <Chip size="small" color="error" label={t(task.kind.endsWith('_configure') ? 'Profile change failed.' : 'Installation failed.')}/>}
                        </Box>
                    </Stack>
                    <Stack
                        direction="row"
                        spacing={1}
                        useFlexGap
                        sx={{flexShrink: 0, flexWrap: 'wrap', justifyContent: {xs: 'flex-start', sm: 'flex-end'}}}
                    >
                        {selectedInstalled ? selectedRunning ? (
                            <Button variant="contained" color="warning" startIcon={isThisAppLoading ? <CircularProgress size={16}/> : <StopCircle/>} onClick={() => void controller.stop()} disabled={disableRowActions}>{t("Stop App")}</Button>
                        ) : (
                            <Button variant="contained" color="success" startIcon={isThisAppLoading ? <CircularProgress size={16}/> : <PlayArrow/>} onClick={startApp} disabled={disableRowActions || !app.current_version}>{t("Start App")}</Button>
                        ) : (
                            <Button variant="contained" color="primary" startIcon={isThisAppLoading ? <CircularProgress size={16}/> : <Build/>} endIcon={<KeyboardArrowRight/>} onClick={onInstall} disabled={disableRowActions || app.running}>{t("Install")}</Button>
                        )}
                        {(task || app.running || state.request === 'start' || state.applicationLogs.length > 0 || app.source_operation_state === 'failed' || app.update_state === 'failed') && (
                            <Button variant="outlined" color="info" size="small" startIcon={<OpenInNew/>} onClick={onConsole}>{t('Console')}</Button>
                        )}
                        {selectedInstalled && app.update_source === 'git' && app.show_add_defender && !state.defenderHidden && <Button variant="outlined" color="secondary" size="small" startIcon={isThisAppLoading && state.request === 'defender' ? <CircularProgress size={16}/> : <Build/>} onClick={() => controller.defender()} disabled={disableRowActions}>{t("Add Defender Exclusion")}</Button>}
                        {selectedInstalled && app.update_source === 'git' && !selectedRunning && app.profiles?.length > 1 && <Button variant="outlined" color="secondary" size="small" startIcon={isThisAppLoading ? <CircularProgress size={16}/> : <Cached/>} onClick={() => onProfileChange()} disabled={disableRowActions}>{t("Change Profile")}</Button>}
                        {selectedInstalled && (
                            <Tooltip title={t('Delete')}>
                                <span>
                                    <IconButton color="error" onClick={() => onDelete()} disabled={disableRowActions || selectedRunning} sx={{border: 1, borderColor: 'divider', borderRadius: 2}}>
                                        {isThisAppLoading ? <CircularProgress size={18}/> : <Delete fontSize="small"/>}
                                    </IconButton>
                                </span>
                            </Tooltip>
                        )}
                        <Tooltip title={t('Settings')}>
                            <IconButton
                                onClick={() => onSettings()}
                                color="inherit"
                                sx={{border: 1, borderColor: 'divider', borderRadius: 2}}
                            >
                                <SettingsIcon fontSize="small"/>
                            </IconButton>
                        </Tooltip>
                    </Stack>
                </Stack>
                {(selectedInstalled || !!inlineConsole) && (!selectedRunning || !!inlineConsole) && (
                    <Box
                        sx={{
                            mt: 2.5,
                            p: {xs: 1.5, sm: 2},
                            borderRadius: 3,
                            bgcolor: theme => alpha(theme.palette.primary.main, theme.palette.mode === 'dark' ? 0.075 : 0.045),
                            flex: inlineConsole ? '1 1 auto' : undefined,
                            minHeight: inlineConsole ? 0 : undefined,
                            display: inlineConsole ? 'flex' : 'block',
                            flexDirection: inlineConsole ? 'column' : undefined,
                        }}
                    >
                        {selectedInstalled && !selectedRunning && (
                            <Stack direction={{xs: 'column', sm: 'row'}} spacing={1} sx={{alignItems: {xs: 'stretch', sm: 'center'}}}>
                                <FormControl size="small" sx={{minWidth: {xs: '100%', sm: 220}}} disabled={disableUpdateControls}>
                                    <InputLabel>{t('Update Method')}</InputLabel>
                                    <Select
                                        value={app.update_method}
                                        label={t('Update Method')}
                                        onChange={(event) => controller.preferences(event.target.value, null)}
                                    >
                                        {UPDATE_METHOD_OPTIONS.filter(option => app.update_source !== 'mirrorchyan' || app.mirrorchyan?.prerelease_channel || option !== 'AUTO_UPDATE_PRE_RELEASE').map(option => (
                                            <MenuItem key={option} value={option}>{t(option)}</MenuItem>
                                        ))}
                                    </Select>
                                </FormControl>
                                {app.available_versions.filter(v => v !== app.current_version).length > 0 ? (
                                    <FormControl size="small" sx={{minWidth: {xs: '100%', sm: 220}}} disabled={disableUpdateControls}>
                                        <InputLabel>{t('Change version...')}</InputLabel>
                                        <Select
                                            value={state.proposal?.version !== app.current_version && app.available_versions.includes(state.proposal?.version ?? '') ? state.proposal?.version ?? '' : ''}
                                            label={t('Change version...')}
                                            renderValue={(selected) => {
                                                const selectedVersion = String(selected);
                                                return selectedVersion ? `${selectedVersion} ${t(getVersionChannelLabelKey(selectedVersion))}` : t('Change version...');
                                            }}
                                            onChange={(e) => controller.selectVersion(e.target.value)}
                                        >
                                            <MenuItem value=""><em>{t('Change version...')}</em></MenuItem>
                                            {app.available_versions.filter(v => v !== app.current_version).map(v => <MenuItem key={v} value={v}>{v} {t(getVersionChannelLabelKey(v))}{compareVersions(v, app.current_version!) > 0 ? ` ${t('(Upgrade)')}` : ` ${t('(Downgrade)')}`}</MenuItem>)}
                                        </Select>
                                    </FormControl>
                                ) : <Typography variant="caption">{t("No other versions found.")}</Typography>}
                                <Tooltip title={t("Check for updates")}><span><IconButton onClick={() => controller.refresh()} disabled={disableRowActions} sx={{bgcolor: 'background.paper', border: 1, borderColor: 'divider', borderRadius: 2}}>{isThisAppLoading && state.request === 'refresh' ? <CircularProgress size={20}/> : <Cached fontSize="small"/>}</IconButton></span></Tooltip>
                            </Stack>
                        )}
                        {inlineConsole === 'task' && task ? (
                            <TaskConsole task={task} inline onBack={onCloseConsole} onCancel={controller.cancel}/>
                        ) : inlineConsole === 'start' ? (
                            <ConsolePage inline title={t('Starting App: {{appName}}', {appName: app.name})} appName={app.name}
                                logs={state.applicationLogs} outcome={state.applicationOutcome} onBack={onCloseConsole} isProcessing={app.running || state.request === 'start'}/>
                        ) : inlineUpdateEntry && (
                            <UpdateLogPage appName={app.name} version={inlineUpdateEntry.version} actionType={inlineUpdateEntry.actionType}
                                isConfirming={taskActive && task?.target_version === inlineUpdateEntry.version}
                                completed={task?.status === 'succeeded' && task.target_version === inlineUpdateEntry.version}
                                failed={task?.status === 'failed' && task.target_version === inlineUpdateEntry.version || app.update_state === 'failed' && app.update_target_version === inlineUpdateEntry.version}
                                website={app.website} onConfirm={confirmVersion} onOpenConsole={onConsole} onCancel={() => controller.selectVersion('')}/>
                        )}
                    </Box>
                )}
            </CardContent>
        </Card>
    );
}
