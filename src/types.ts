import type {VersionActionType} from './updateProgress';
export type OperationKind = 'git_install' | 'git_update' | 'git_configure' | 'mirror_install' | 'mirror_update' | 'mirror_configure';
export type OperationStatus = 'pending' | 'running' | 'cancelling' | 'succeeded' | 'failed' | 'cancelled';
export interface Operation {
    id: string; sequence: number; app_name: string; kind: OperationKind; status: OperationStatus; can_cancel: boolean;
    target_version: string | null; previous_version?: string | null; profile: string | null; error: string | null;
}
export interface MessagePayload {
    app_name: string; operation_id?: string | null; message: string;
    update?: boolean; finished?: boolean; error?: boolean; cancelled?: boolean;
}
export interface MirrorProgress { operation_id?: string | null; phase: string; downloaded: number; total: number | null; }
export interface Task extends Operation { logs: MessagePayload[]; progress?: MirrorProgress & {value: number}; action: VersionActionType; notes?: string; }
export type SettingsRequest = <T>(operation: () => Promise<T>) => Promise<T>;
export type ThemeModeSetting = 'light' | 'dark' | 'system';
export type StatusState = {loading?: boolean; error?: string | null; info?: string | null; messageLoading?: boolean};
export type VersionProposal = {version: string; actionType: VersionActionType};
export interface Profile {
    name: string;
    main_script: string;
    admin: boolean;
    requirements: string;
    python_path: string;
}

export interface App {
    revision: number;
    operation?: Operation | null;
    installation?: {source: 'git' | 'mirrorchyan'; version: string | null} | null;
    mirrorchyan: {resource_id: string; stable_channel: string; prerelease_channel?: string | null} | null;
    update_source: 'git' | 'mirrorchyan';
    name: string;
    icon: string;
    website: string | null;
    path: string;
    current_version: string | null;
    available_versions: string[];
    running: boolean;
    installed: boolean;
    update_method: string;
    auto_start: boolean;
    update_state: 'idle' | 'updating' | 'failed';
    source_operation_state: 'idle' | 'updating' | 'failed';
    source_operation_kind?: OperationKind | null;
    source_operation_target?: string | null;
    source_operation_error?: string | null;
    update_target_version: string | null;
    update_error: string | null;
    profiles: Profile[];
    current_profile: string;
    show_add_defender: boolean;
}
