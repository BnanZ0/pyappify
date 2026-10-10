// src/UpdateLogPage.tsx
import React, {useEffect, useState} from 'react';
import {invoke} from "@tauri-apps/api/core";
import {Alert, Box, Button, CircularProgress, Link, Paper, Stack, Typography} from "@mui/material";
import {openUrl} from '@tauri-apps/plugin-opener';
import {useTranslation} from 'react-i18next';
import CheckCircleOutlineIcon from '@mui/icons-material/CheckCircleOutlineOutlined';
import ErrorOutlineIcon from '@mui/icons-material/ErrorOutlineOutlined';
import type {VersionActionType} from './updateProgress';

interface UpdateLogPanelProps {
    appName: string;
    version: string;
    actionType: VersionActionType;
    isConfirming?: boolean;
    completed?: boolean;
    failed?: boolean;
    website?: string | null;
    onConfirm: (params: { appName: string, version: string, actionType: VersionActionType; notes?: string }) => void;
    onCancel: () => void;
    onOpenConsole: () => void;
}

const UpdateLogPage: React.FC<UpdateLogPanelProps> = ({
                                                          appName,
                                                          version,
                                                          actionType,
                                                          isConfirming = false,
                                                          completed = false,
                                                          failed = false,
                                                          website,
                                                          onConfirm,
                                                          onCancel,
                                                          onOpenConsole,
                                                      }) => {
    const {t} = useTranslation();
    const [notes, setNotes] = useState<string | null>(null);
    const [notesLoading, setNotesLoading] = useState(true);
    const [notesError, setNotesError] = useState<string | null>(null);

    useEffect(() => {
        let disposed = false;
        const fetchNotes = async () => {
            setNotesLoading(true);
            setNotes(null);
            setNotesError(null);
            try {
                const fetchedNotes = await invoke<string[]>("get_update_notes", {appName, version});
                if (!disposed) setNotes(fetchedNotes.join("\n"));
            } catch (err) {
                console.error(`Failed to get notes for ${appName} version ${version}:`, err);
                const errorMessage = typeof err === 'object' && err !== null && 'message' in err ? String(err.message) : String(err);
                if (!disposed) setNotesError(t('Failed to load notes: {{error}}', {error: errorMessage}));
            } finally {
                if (!disposed) setNotesLoading(false);
            }
        };

        if (appName && version) {
            fetchNotes();
        }
        return () => {disposed = true;};
    }, [appName, version, t]);

    const handleConfirm = () => onConfirm({appName, version, actionType, notes: notes ?? undefined});

    const handleOpenWebsite = async () => {
        const target = website?.trim();
        if (!target) return;
        try {
            await openUrl(target);
        } catch (error) {
            console.warn('Failed to open app website:', error);
        }
    };

    const progressTranslationKey = actionType === 'Upgrade'
        ? 'Upgrading...'
        : actionType === 'Downgrade'
            ? 'Downgrading...'
            : 'Setting...';
    const confirmButtonText = isConfirming
        ? t(progressTranslationKey)
        : t('Confirm {{actionType}}', {actionType: t(actionType)});

    let pageTitle: string;
    let borderColor: string;
    let titleColor: string;
    let titleIcon: React.ReactNode = null;

    if (completed) {
        pageTitle = `${t(`${actionType} success`)}: ${version}`;
        borderColor = 'success.main';
        titleColor = 'success.main';
        titleIcon = <CheckCircleOutlineIcon fontSize="small" color="success"/>;
    } else if (failed) {
        pageTitle = `${t(`${actionType} failed`)}: ${version}`;
        borderColor = 'error.main';
        titleColor = 'error.main';
        titleIcon = <ErrorOutlineIcon fontSize="small" color="error"/>;
    } else if (isConfirming) {
        pageTitle = `${t(progressTranslationKey)}: ${version}`;
        borderColor = 'info.main';
        titleColor = 'info.main';
        titleIcon = <CircularProgress size={14}/>;
    } else {
        pageTitle = `${t(actionType)}: ${version}`;
        borderColor = 'divider';
        titleColor = 'text.primary';
    }

    return (
        <Box sx={{mt: 2, border: 1, borderColor, borderRadius: 1, p: 2}}>
            <Stack direction="row" spacing={0.5} sx={{mb: 0.5, alignItems: 'center'}}>
                {isConfirming || failed ? (
                    <Button
                        variant="text"
                        size="small"
                        color={failed ? 'error' : 'info'}
                        startIcon={titleIcon}
                        onClick={onOpenConsole}
                        sx={{fontWeight: 'bold', color: titleColor}}
                    >
                        {pageTitle}
                    </Button>
                ) : (
                    <>
                        {titleIcon}
                        <Typography variant="subtitle1" sx={{fontWeight: 'bold', color: titleColor}}>
                            {pageTitle}
                        </Typography>
                    </>
                )}
            </Stack>

            {notesLoading && (
                <Box sx={{display: 'flex', alignItems: 'center', my: 1}}>
                    <CircularProgress size={18} sx={{mr: 1}}/>
                    <Typography variant="body2">{t('Loading notes...')}</Typography>
                </Box>
            )}
            {notesError && (
                <Alert severity="error" sx={{my: 1}}>
                    {notesError}
                </Alert>
            )}

            {failed && actionType === 'Upgrade' && website?.trim() && (
                <Alert severity="warning" sx={{my: 1}}>
                    {t('Upgrade failed. Please visit the website to download the latest version.')}{' '}
                    <Link component="button" type="button" onClick={handleOpenWebsite}>
                        {t('Open website')}
                    </Link>
                </Alert>
            )}

            {notes && !notesLoading && !notesError && (
                <Paper elevation={0} variant="outlined" sx={{
                    p: 1.5,
                    mt: 1,
                    whiteSpace: 'pre-wrap',
                    fontFamily: 'monospace',
                    fontSize: '0.8rem',
                    maxHeight: '200px',
                    overflowY: 'auto',
                    bgcolor: 'action.hover',
                }}>
                    {notes}
                </Paper>
            )}

            {/* Failed updates can be confirmed again so the user can retry. */}
            {!notesLoading && !completed && !isConfirming && (
                <Stack direction="row" spacing={1} sx={{mt: 2, justifyContent: 'flex-end'}}>
                    <Button
                        variant="outlined"
                        size="small"
                        onClick={onCancel}
                    >
                        {t('Cancel')}
                    </Button>
                    <Button
                        variant="contained"
                        size="small"
                        color={actionType === 'Upgrade' ? 'success' : 'warning'}
                        onClick={handleConfirm}
                        disabled={notesLoading || !!notesError}
                    >
                        {confirmButtonText}
                    </Button>
                </Stack>
            )}
        </Box>
    );
};

export default UpdateLogPage;
