import {useEffect, useRef, useState} from 'react';
import {Alert, FormControl, InputLabel, MenuItem, Select, Button, CircularProgress, Stack, TextField, Typography} from '@mui/material';
import {openUrl} from '@tauri-apps/plugin-opener';
import {invoke} from '@tauri-apps/api/core';
import {useTranslation} from 'react-i18next';

import type {SettingsRequest} from '../../types';

export type MirrorSettingsApp = {name: string; update_source: 'git' | 'mirrorchyan'; update_state: string; source_operation_state: string; mirrorchyan: {resource_id: string; prerelease_channel?: string | null} | null};

export default function MirrorSettings({app, busy, saving, runRequest}: {app: MirrorSettingsApp | null; busy: boolean; saving: boolean; runRequest: SettingsRequest}) {
    const {t} = useTranslation();
    const [cdk, setCdk] = useState('');
    const [hasCdk, setHasCdk] = useState(false);
    const querySequence = useRef(0);
    const [mirrorError, setMirrorError] = useState('');
    useEffect(() => {
        const generation = ++querySequence.current;
        if (app?.mirrorchyan || app?.update_source === 'mirrorchyan') {
            invoke<boolean>('mirrorchyan_has_cdk').then(value => {if (generation === querySequence.current) setHasCdk(value);}).catch(() => {if (generation === querySequence.current) setMirrorError(t('mirrorSettingsFailed'));});
        }
        return () => {querySequence.current++;};
    }, [app?.name, app?.mirrorchyan?.resource_id, app?.update_source, t]);

    const changeMirrorSetting = async (operation: () => Promise<unknown>) => {
        const generation = ++querySequence.current;
        setMirrorError('');
        try {
            await runRequest(async () => {
                await operation();
                setCdk('');
                const saved = await invoke<boolean>('mirrorchyan_has_cdk');
                if (generation === querySequence.current) setHasCdk(saved);
                // Source selection is local and must remain usable while versions load.
                void invoke('load_app').catch((error) => {
                    if (generation !== querySequence.current) return;
                    const detail = typeof error === 'string' ? error : (error as {message?: string})?.message;
                    setMirrorError(detail || t('mirrorSettingsFailed'));
                });
            });
        } catch (error) {
            const detail = typeof error === 'string' ? error : (error as {message?: string})?.message;
            setMirrorError(detail || t('mirrorSettingsFailed'));
        }
    };

    return <>
                {app && (app.mirrorchyan || app.update_source === 'mirrorchyan') && <Stack spacing={2} sx={{mt: 3, mb: 2}}>
                    <FormControl fullWidth disabled={busy}>
                        <InputLabel>{t('updateSource')}</InputLabel>
                        <Select value={app.update_source || 'git'} label={t('updateSource')}
                            onChange={e => void changeMirrorSetting(() => invoke('update_app_preferences', {appName: app.name, updateSource: e.target.value}))}>
                            <MenuItem value="git">Git + pip</MenuItem>
                            <MenuItem value="mirrorchyan" disabled={!app.mirrorchyan}>{t('mirrorName')}</MenuItem>
                        </Select>
                    </FormControl>
                    {app.update_source === 'mirrorchyan' && <>
                        <Alert severity="info">{t('mirrorUpdateInfo')}</Alert>
                        {!app.mirrorchyan?.prerelease_channel && <Typography variant="body2">{t('mirrorStableOnly')}</Typography>}
                        <TextField type="password" label={`${t('mirrorName')} CDK`} value={cdk} autoComplete="off"
                            disabled={busy} onChange={(e: React.ChangeEvent<HTMLInputElement>) => setCdk(e.target.value)}
                            helperText={t(hasCdk ? 'mirrorCdkSaved' : 'mirrorCdkMissing')}/>
                        <Stack direction="row" spacing={1}>
                            <Button disabled={busy || !cdk.trim()} onClick={() => void changeMirrorSetting(() => invoke('mirrorchyan_set_cdk', {cdk}))}>{t('mirrorSaveCdk')}</Button>
                            <Button disabled={busy || !hasCdk} onClick={() => void changeMirrorSetting(() => invoke('mirrorchyan_set_cdk', {cdk: ''}))}>{t('mirrorClearCdk')}</Button>
                            <Button onClick={() => void openUrl('https://mirrorchyan.com').catch(() => setMirrorError(t('mirrorSettingsFailed')))}>{t('mirrorName')} ↗</Button>
                        </Stack>
                    </>}
                    {saving && <CircularProgress size={20}/>}
                    {mirrorError && <Alert severity="error">{mirrorError}</Alert>}
                </Stack>}
    </>;
}
