export const mirrorPhaseLabel = (phase: string): string => ({
    downloading: 'Downloading update...', downloaded: 'Update downloaded and verified.',
    extracting: 'Extracting update...', installing: 'Installing application...', completed: 'Installation completed.',
    download_failed: 'Update download failed.', install_failed: 'Installation failed.', cancelled: 'Operation cancelled.',
} as Record<string, string>)[phase] ?? 'Preparing installation...';
