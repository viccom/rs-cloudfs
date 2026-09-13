// Volume management page (web volume management plan §1.4): the
// configuration-state twin of the usage-state dashboard. P0 is
// read-only — one /api/volumes poll every 4s (the app.js cadence)
// redraws the stat cards, the storage card (instance-level Σ, 裁决⑤)
// and the volume table; the Add Volume button and the Actions column
// are placeholders until the P1+ write batches land.

const VOLUME_LABELS = {
    telegram: 'Telegram MTProto',
    baidu: 'Baidu Netdisk',
    local: 'Local Disk',
};

function backendDisplayName(backend) {
    return VOLUME_LABELS[backend] || backend || 'unknown';
}

document.addEventListener("DOMContentLoaded", () => {
    loadVolumesPage();
    setInterval(loadVolumesPage, 4000);
});

async function loadVolumesPage() {
    try {
        const res = await fetch("/api/volumes");
        if (!res.ok) {
            renderVolumesError(res.status);
            return;
        }
        const volumes = await res.json();
        renderVolumeCards(volumes);
        renderStorageCard(volumes);
        renderVolumesTable(volumes);
    } catch (err) {
        console.error("Error loading volumes:", err);
    }
}

function renderVolumesError(status) {
    const tbody = document.getElementById("volumes-tbody");
    if (!tbody) return;
    tbody.innerHTML = `
        <tr>
            <td colspan="7" style="text-align: center; color: var(--text-muted); padding: 3rem;">
                <i class="fa-solid fa-triangle-exclamation" style="font-size: 2rem; display: block; color: #ff6688; margin-bottom: 0.8rem;"></i>
                The volume listing failed (HTTP ${status}) — this page needs a multi-volume instance.
            </td>
        </tr>
    `;
}

// The four stat cards: 卷总数 / Running / Failed / Pending 总和 (the
// pending sum counts only the volumes that have a queue — failed ones
// have none, not zero).
function renderVolumeCards(volumes) {
    const running = volumes.filter(v => v.status === 'running').length;
    const failed = volumes.filter(v => v.status === 'failed').length;
    const pending = volumes.reduce(
        (sum, v) => sum + (typeof v.pending === "number" ? v.pending : 0), 0);
    setText("stat-volume-count", volumes.length);
    setText("stat-volume-running", running);
    setText("stat-volume-failed", failed);
    setText("stat-volume-pending", pending);
}

// Instance-level storage (裁决⑤): Σ of every volume's indexed bytes,
// with the quota ratio when the volumes report quota ceilings (the
// index storage card's visual language, fed by the whole registry).
function renderStorageCard(volumes) {
    let bytes = 0, quotaUsed = 0, quotaTotal = 0, hasQuota = false;
    for (const v of volumes) {
        if (typeof v.total_bytes === "number") bytes += v.total_bytes;
        if (typeof v.quota_used === "number") quotaUsed += v.quota_used;
        if (typeof v.quota_total === "number") {
            quotaTotal += v.quota_total;
            hasQuota = true;
        }
    }
    const detailEl = document.getElementById("storage-detail");
    const barEl = document.getElementById("storage-bar");
    const percentEl = document.getElementById("storage-percent");
    const indexed = formatBytes(bytes);
    if (detailEl && barEl && percentEl) {
        if (hasQuota && quotaTotal > 0) {
            const pct = Math.min(100, Math.round((quotaUsed / quotaTotal) * 100));
            detailEl.innerText = `${formatBytes(quotaUsed)} / ${formatBytes(quotaTotal)}`;
            percentEl.innerText = `${pct}%`;
            barEl.style.width = `${pct}%`;
        } else {
            detailEl.innerText = `${indexed} / Unlimited`;
            percentEl.innerText = '∞';
            barEl.style.width = '0%';
        }
    }
    setText("volume-count", volumes.length);
}

function renderVolumesTable(volumes) {
    const tbody = document.getElementById("volumes-tbody");
    const countLabel = document.getElementById("volume-count-label");
    if (countLabel) countLabel.innerText = `${volumes.length} volume${volumes.length === 1 ? "" : "s"}`;
    if (!tbody) return;

    if (!volumes.length) {
        tbody.innerHTML = `
            <tr>
                <td colspan="7" style="text-align: center; color: var(--text-muted); padding: 3rem;">
                    <i class="fa-solid fa-database" style="font-size: 2.2rem; margin-bottom: 0.8rem; display: block; color: var(--accent-cyan); opacity: 0.6;"></i>
                    No volumes are registered on this instance.
                </td>
            </tr>
        `;
        return;
    }

    tbody.innerHTML = volumes.map(v => {
        const failed = v.status === 'failed';
        const statusBadge = failed
            ? `<span class="badge-status badge-failed" title="${escapeHtml(v.status_reason || 'assembly failed')}"><i class="fa-solid fa-circle-exclamation"></i> Failed</span>`
            : '<span class="badge-status badge-synced"><i class="fa-solid fa-circle-check"></i> Running</span>';
        const drive = v.drive_letter || '-';
        const pending = typeof v.pending === "number" ? v.pending : '-';
        const size = typeof v.total_bytes === "number" ? formatBytes(v.total_bytes) : '—';
        return `
            <tr title="${failed ? escapeHtml(v.status_reason || '') : ''}">
                <td>
                    <div class="file-name-cell">
                        <i class="fa-solid fa-database" style="color: var(--accent-cyan); font-size: 1.1rem;"></i>
                        <span>${escapeHtml(v.name)}</span>
                    </div>
                </td>
                <td>${escapeHtml(backendDisplayName(v.backend))}</td>
                <td>${statusBadge}</td>
                <td>${escapeHtml(drive)}</td>
                <td>${pending}</td>
                <td>${size}</td>
                <td></td>
            </tr>
        `;
    }).join("");
}

function setText(id, text) {
    const el = document.getElementById(id);
    if (el) el.innerText = text;
}

function formatBytes(bytes, decimals = 2) {
    if (!bytes) return '0 Bytes';
    const k = 1024;
    const dm = decimals < 0 ? 0 : decimals;
    const sizes = ['Bytes', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(dm)) + ' ' + sizes[i];
}

function escapeHtml(text) {
    return String(text).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
