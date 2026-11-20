import {useEffect, useState, useSyncExternalStore} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {listen} from '@tauri-apps/api/event';
import type {App} from '../../types';
import type {TFunction} from 'i18next';
import {createLauncherController} from './controller';

export function useLauncherController(t: TFunction, onChooseProfile: (app: App) => void) {
    const [controller] = useState(() => createLauncherController({invoke, listen}, t));
    controller.setTranslator(t);
    const state = useSyncExternalStore(controller.subscribe, controller.getSnapshot);
    useEffect(() => {
        const connection = controller.connect(onChooseProfile);
        return () => {controller.disconnect(); void connection.then(disconnect => disconnect());};
    }, [controller, onChooseProfile]);
    return {controller, ...state};
}
