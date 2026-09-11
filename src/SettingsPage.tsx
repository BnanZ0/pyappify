// src/SettingsPage.tsx
import React, {useEffect, useState} from 'react';
import {
    Box,
    Button,
    CircularProgress,
    Container,
    FormControl,
    InputLabel,
    MenuItem,
    Paper,
    Select,
    SelectChangeEvent,
    Typography
} from '@mui/material';
import {Alert, TextField, Stack} from '@mui/material';
import {openUrl} from '@tauri-apps/plugin-opener';
import i18n from "i18next";
import {useTranslation} from 'react-i18next';
import {invoke} from "@tauri-apps/api/core";
import {invokeTauriCommandWrapper} from "./utils.ts";
import {ThemeModeSetting} from "./App.tsx";

interface StatusUpdateProps {
    updateStatus: (newStatus: { error?: string | null, info?: string | null, messageLoading?: boolean }) => void;
    clearMessages: () => void;
}

interface SettingsPageProps extends StatusUpdateProps {
    app: {name: string; update_source: 'git' | 'mirrorchyan'; update_state: string; mirrorchyan: {resource_id: string; prerelease_channel?: string | null} | null} | null;
    currentTheme: ThemeModeSetting;
    onChangeTheme: (theme: ThemeModeSetting) => void;
    onBack: () => void;
}

interface ConfigItemFromRust {
    name: string;
    description: string;
    value: string | number | boolean;
    default_value: string | number | boolean;
    options?: (string | number | boolean)[];
}
const PIP_CACHE_DIR_CONFIG_KEY = "Pip Cache Directory";
const PIP_INDEX_URL_CONFIG_KEY = "Pip Index URL";
const LANGUAGE_CONFIG_KEY = "Language";

const languageNames: { [key: string]: string } = { 'en': 'English', 'zh-CN': '简体中文', 'zh-TW': '繁體中文', 'es': 'Español', 'ja': '日本語', 'ko': '한국인' };

const getPipIndexUrlName = (url: string, t: (key: string) => string) => {
    if (url === '') return t('System Default');
    if (url.includes('pypi.org')) return t('PyPI');
    if (url.includes('tsinghua')) return t('Tsinghua');
    if (url.includes('aliyun')) return t('Aliyun');
    if (url.includes('ustc')) return t('USTC');
    if (url.includes('huaweicloud')) return t('Huawei Cloud');
    if (url.includes('tencent')) return t('Tencent Cloud');
    return url;
};

const SettingsPage: React.FC<SettingsPageProps> = ({ app, currentTheme, onChangeTheme, onBack, updateStatus, clearMessages }) => {
    const {t} = useTranslation();
    const [configs, setConfigs] = useState<ConfigItemFromRust[] | null>(null);
    const [isLoading, setIsLoading] = useState(true);
    const [cdk, setCdk] = useState('');
    const [hasCdk, setHasCdk] = useState(false);
    const [mirrorBusy, setMirrorBusy] = useState(false);
    const [mirrorError, setMirrorError] = useState('');
    useEffect(() => {
        if (app?.mirrorchyan || app?.update_source === 'mirrorchyan') {
            invoke<boolean>('mirrorchyan_has_cdk').then(setHasCdk).catch(() => setMirrorError(t('mirrorSettingsFailed')));
        }
    }, [app?.name, app?.mirrorchyan?.resource_id, app?.update_source, t]);

    const changeMirrorSetting = async (operation: () => Promise<unknown>) => {
        setMirrorBusy(true);
        setMirrorError('');
        try {
            await operation();
            setCdk('');
            setHasCdk(await invoke<boolean>('mirrorchyan_has_cdk'));
            await invoke('load_app');
        } catch (error) {
            const detail = typeof error === 'string' ? error : (error as {message?: string})?.message;
            setMirrorError(detail || t('mirrorSettingsFailed'));
        } finally { setMirrorBusy(false); }
    };

    const loadConfigs = async () => {
        setIsLoading(true);
        await invokeTauriCommandWrapper<ConfigItemFromRust[]>(
            'get_config_payload', undefined,
            (result) => {
                setConfigs(result);
                const languageConfig = result.find(c => c.name === LANGUAGE_CONFIG_KEY);
                if (languageConfig && languageConfig.value) {
                    i18n.changeLanguage(languageConfig.value as string);
                }
            },
            (errorMsg) => updateStatus({error: `Failed to load settings: ${errorMsg}`})
        );
        setIsLoading(false);
    };

    useEffect(() => {
        loadConfigs();
    }, []);

    const handleSettingChange = async (name: string, value: string | number) => {
        clearMessages();
        updateStatus({messageLoading: true});
        await invokeTauriCommandWrapper<void>(
            'update_config_item', {name, value},
            async () => {
                const updatedConfigs = await invoke<ConfigItemFromRust[]>('get_config_payload');
                setConfigs(updatedConfigs);
                if (name === LANGUAGE_CONFIG_KEY) i18n.changeLanguage(value as string);
                updateStatus({info: `${name} updated successfully.`, messageLoading: false});
            },
            (errorMsg) => updateStatus({error: `Failed to update ${name}: ${errorMsg}`, messageLoading: false})
        );
    };

    if (isLoading || !configs) {
        return (
            <Container maxWidth="sm" sx={{py: 4, display: 'flex', justifyContent: 'center', alignItems: 'center', height: '100vh'}}>
                <CircularProgress/><Typography sx={{ml: 2}}>{t('Loading settings...')}</Typography>
            </Container>
        );
    }

    const getConfig = (key: string) => configs.find(c => c.name === key);
    const languageConfig = getConfig(LANGUAGE_CONFIG_KEY);
    const themeConfig = { value: currentTheme, options: ['system', 'light', 'dark'] };
    const pipCacheConfig = getConfig(PIP_CACHE_DIR_CONFIG_KEY);
    const pipIndexUrlConfig = getConfig(PIP_INDEX_URL_CONFIG_KEY);

    return (
        <Container maxWidth="sm" sx={{py: 4}}>
            <Paper elevation={3} sx={{p: 3}}>
                <Typography variant="h4" component="h1" gutterBottom align="center">{t('Settings')}</Typography>
                {[
                    { label: t('Language'), config: languageConfig, handler: (e: SelectChangeEvent) => handleSettingChange(LANGUAGE_CONFIG_KEY, e.target.value), renderOption: (o: string) => languageNames[o] || o },
                    { label: t('Theme'), config: themeConfig, handler: (e: SelectChangeEvent) => onChangeTheme(e.target.value as ThemeModeSetting), renderOption: (o: string) => t(o.charAt(0).toUpperCase() + o.slice(1)) },
                ].map(({ label, config, handler, renderOption }) => config && (
                    <Box key={label} sx={{my: 2}}>
                        <FormControl fullWidth variant="outlined">
                            <InputLabel>{label}</InputLabel>
                            <Select value={(config.value as string) || ''} label={label} onChange={handler}>
                                {(config.options as string[])?.map(o => <MenuItem key={o} value={o}>{renderOption(o)}</MenuItem>)}
                            </Select>
                        </FormControl>
                    </Box>
                ))}
                {app && (app.mirrorchyan || app.update_source === 'mirrorchyan') && <Stack spacing={2} sx={{mt: 3, mb: 2}}>
                    <FormControl fullWidth disabled={mirrorBusy || app.update_state === 'updating'}>
                        <InputLabel>{t('updateSource')}</InputLabel>
                        <Select value={app.update_source || 'git'} label={t('updateSource')}
                            onChange={e => void changeMirrorSetting(() => invoke('update_app_preferences', {appName: app.name, updateSource: e.target.value}))}>
                            <MenuItem value="git">Git + pip</MenuItem>
                            <MenuItem value="mirrorchyan" disabled={!app.mirrorchyan}>Mirror酱</MenuItem>
                        </Select>
                    </FormControl>
                    {app.update_source === 'mirrorchyan' && <>
                        <Alert severity="info">{t('mirrorInstallerInfo')}</Alert>
                        {!app.mirrorchyan?.prerelease_channel && <Typography variant="body2">{t('mirrorStableOnly')}</Typography>}
                        <TextField type="password" label="Mirror酱 CDK" value={cdk} autoComplete="off"
                            disabled={mirrorBusy || app.update_state === 'updating'} onChange={e => setCdk(e.target.value)}
                            helperText={t(hasCdk ? 'mirrorCdkSaved' : 'mirrorCdkMissing')}/>
                        <Stack direction="row" spacing={1}>
                            <Button disabled={mirrorBusy || app.update_state === 'updating' || !cdk.trim()} onClick={() => void changeMirrorSetting(() => invoke('mirrorchyan_set_cdk', {cdk}))}>{t('mirrorSaveCdk')}</Button>
                            <Button disabled={mirrorBusy || app.update_state === 'updating' || !hasCdk} onClick={() => void changeMirrorSetting(() => invoke('mirrorchyan_set_cdk', {cdk: ''}))}>{t('mirrorClearCdk')}</Button>
                            <Button onClick={() => void openUrl('https://mirrorchyan.com').catch(() => setMirrorError(t('mirrorSettingsFailed')))}>Mirror酱 ↗</Button>
                        </Stack>
                    </>}
                    {mirrorBusy && <CircularProgress size={20}/>}
                    {mirrorError && <Alert severity="error">{mirrorError}</Alert>}
                </Stack>}
                {app?.update_source !== 'mirrorchyan' && [
                    { label: t('Pip Cache Directory'), config: pipCacheConfig, handler: (e: SelectChangeEvent) => handleSettingChange(PIP_CACHE_DIR_CONFIG_KEY, e.target.value), renderOption: (o: string) => t(o) },
                    { label: t('Pip Index URL'), config: pipIndexUrlConfig, handler: (e: SelectChangeEvent) => handleSettingChange(PIP_INDEX_URL_CONFIG_KEY, e.target.value), renderOption: (o: string) => getPipIndexUrlName(o, t) },
                ].map(({ label, config, handler, renderOption }) => config && (
                    <Box key={label} sx={{my: 2}}>
                        <FormControl fullWidth variant="outlined">
                            <InputLabel>{label}</InputLabel>
                            <Select value={(config.value as string) || ''} label={label} onChange={handler}>
                                {(config.options as string[])?.map(o => <MenuItem key={o} value={o}>{renderOption(o)}</MenuItem>)}
                            </Select>
                        </FormControl>
                    </Box>
                ))}
                <Box sx={{mt: 4, display: 'flex', justifyContent: 'center'}}>
                    <Button variant="outlined" onClick={onBack}>{t('Back to App')}</Button>
                </Box>
            </Paper>
        </Container>
    );
};
export default SettingsPage;
