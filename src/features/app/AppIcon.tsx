import {useEffect, useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {Box} from '@mui/material';
type AppIconAsset = {
    bytes: number[];
    mime_type: string;
};

export default function AppIcon({appName, iconPath}: {appName: string; iconPath: string}) {
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
