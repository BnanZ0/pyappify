import type {App, OperationKind} from '../../types';
export const installedSource = (app: App) => app.installation?.source ?? 'git';
export const sourceNeedsInstallation = (app: App) => app.installed && installedSource(app) !== app.update_source;
export const installationKind = (app: App): OperationKind => app.update_source === 'git'
    ? app.installed && installedSource(app) === 'git' ? 'git_configure' : 'git_install'
    : app.installed && installedSource(app) === 'mirrorchyan' ? 'mirror_configure' : 'mirror_install';
