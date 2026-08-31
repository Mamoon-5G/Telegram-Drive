import { BandwidthStats } from '../../../types';
import { formatBytes } from '../../../utils';

interface BandwidthWidgetProps {
    bandwidth: BandwidthStats | null;
}

export function BandwidthWidget({ bandwidth }: BandwidthWidgetProps) {
    if (!bandwidth) return null;

    const usedBytes = bandwidth.up_bytes;
    const limit = bandwidth.limit_bytes || 1000 * 1024 * 1024 * 1024;
    const percent = Math.min((usedBytes / limit) * 100, 100);

    return (
        <div className="mt-1.5 space-y-1 text-metadata text-app-text-secondary">
            <div className="flex justify-between">
                <span>Uploads today:</span>
            </div>
            <div className="h-1 w-full overflow-hidden rounded-full bg-app-border">
                <div
                    className="h-full rounded-full bg-app-accent transition-[width] duration-500"
                    style={{ width: `${percent}%` }}
                ></div>
            </div>
            <div className="flex justify-between text-badge text-app-text-tertiary">
                <span>{formatBytes(usedBytes)}</span>
                <span>{formatBytes(limit)}</span>
            </div>
        </div>
    );
}
