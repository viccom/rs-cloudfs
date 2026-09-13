// Volume management page (web volume management plan §1.4): the
// configuration-state twin of the usage-state dashboard. The table's
// data source is GET /api/volumes/configs (the configuration FULL set —
// the runtime registry listing cannot see disabled volumes), enriched
// with each running volume's runtime row from GET /api/volumes (drive,
// pending, size, failure reason). The Actions column is live since P1:
// [Unmount]/[Disable] on a running volume, [Enable] on a stopped or
// disabled one — single-step confirm for the two reversible operations,
// a loading state while the command runs, a toast with the backend's
// reply text (ERR text is already actionable), and an immediate refresh
// when it settles. P6 adds [Refresh] (rebuild the volume's index from
// its remote backend, background): no confirm (idempotent, nothing is
// deleted), its loading state driven by the row's `rebuilding` marker
// (the poll data), and a muted tooltip note where a backend cannot be
// rebuilt (telegram's shadow index, encrypted volumes → cydrive sync).

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
    const tbody = document.getElementById("volumes-tbody");
    if (tbody) tbody.addEventListener("click", onVolumeActionClick);
});

// The write actions: which HTTP route each button posts to.
const VOLUME_ACTIONS = {
    unmount: { route: 'remove', confirm: true },
    disable: { route: 'disable', confirm: true },
    enable: { route: 'enable', confirm: false },
    refresh: { route: 'rebuild', confirm: false },
};

async function loadVolumesPage() {
    try {
        // allSettled: a read-only boot without the command seam still
        // renders the runtime rows (the configs fetch 503s alone).
        const [configsRes, runtimeRes] = await Promise.allSettled([
            fetch("/api/volumes/configs"),
            fetch("/api/volumes"),
        ]);
        const configs = configsRes.status === 'fulfilled' && configsRes.value.ok
            ? await configsRes.value.json()
            : null;
        const runtime = runtimeRes.status === 'fulfilled' && runtimeRes.value.ok
            ? await runtimeRes.value.json()
            : null;
        if (!configs && !runtime) {
            renderVolumesError(runtimeRes.status === 'fulfilled'
                ? configsRes.value.status : 0);
            return;
        }
        renderVolumeCards(configs, runtime);
        renderStorageCard(runtime || []);
        renderVolumesTable(configs, runtime || []);
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

// The four stat cards: 卷总数 (configured files — the configs full set
// when available, else the running registry) / Running / Failed /
// Pending 总和 (the pending sum counts only volumes that have a queue —
// failed ones have none, not zero).
function renderVolumeCards(configs, runtime) {
    const runningRows = runtime ? runtime.filter(v => v.status === 'running') : [];
    const failed = runtime ? runtime.filter(v => v.status === 'failed').length : 0;
    const pending = runtime ? runtime.reduce(
        (sum, v) => sum + (typeof v.pending === "number" ? v.pending : 0), 0) : 0;
    const count = configs ? configs.length : (runtime ? runtime.length : 0);
    setText("stat-volume-count", count);
    setText("stat-volume-running", runningRows.length);
    setText("stat-volume-failed", failed);
    setText("stat-volume-pending", pending);
}

// Instance-level storage (裁决⑤): Σ of every running volume's indexed
// bytes, with the quota ratio when the volumes report quota ceilings
// (the index storage card's visual language, fed by the runtime rows).
function renderStorageCard(runtime) {
    let bytes = 0, quotaUsed = 0, quotaTotal = 0, hasQuota = false;
    for (const v of runtime) {
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
    setText("volume-count", runtime.length);
}

// One table row model: the configs row (name/backend/enabled/running or
// invalid, plus the sparse rebuilding/encrypted markers) joined with the
// runtime row of the same name (status detail, drive, pending, size).
// Rows without a configs listing (a read-only boot) fall back to the
// runtime row alone.
function volumeRows(configs, runtime) {
    const runtimeByName = new Map(runtime.map(v => [v.name, v]));
    if (!configs) {
        return runtime.map(v => ({ name: v.name, backend: v.backend, invalid: false, enabled: true, running: v.status === 'running', runtime: v }));
    }
    return configs.map(c => ({
        name: c.name,
        backend: c.backend,
        invalid: c.invalid === true,
        invalidReason: c.reason || '',
        enabled: c.enabled !== false,
        running: c.running === true,
        rebuilding: c.rebuilding === true,
        encrypted: c.encrypted === true,
        runtime: runtimeByName.get(c.name) || null,
    }));
}

function renderVolumesTable(configs, runtime) {
    const tbody = document.getElementById("volumes-tbody");
    const countLabel = document.getElementById("volume-count-label");
    const rows = volumeRows(configs, runtime);
    if (countLabel) countLabel.innerText = `${rows.length} volume${rows.length === 1 ? "" : "s"}`;
    if (!tbody) return;

    if (!rows.length) {
        tbody.innerHTML = `
            <tr>
                <td colspan="7" style="text-align: center; color: var(--text-muted); padding: 3rem;">
                    <i class="fa-solid fa-database" style="font-size: 2.2rem; margin-bottom: 0.8rem; display: block; color: var(--accent-cyan); opacity: 0.6;"></i>
                    No volume files are configured on this instance.
                </td>
            </tr>
        `;
        return;
    }

    tbody.innerHTML = rows.map(v => {
        let statusBadge, rowClass = '', title = '';
        if (v.invalid) {
            statusBadge = `<span class="badge-status badge-failed" title="${escapeHtml(v.invalidReason)}"><i class="fa-solid fa-file-circle-xmark"></i> Invalid</span>`;
            title = escapeHtml(v.invalidReason);
        } else if (v.enabled && v.running) {
            const failed = v.runtime && v.runtime.status === 'failed';
            if (failed) {
                const reason = v.runtime.status_reason || 'assembly failed';
                statusBadge = `<span class="badge-status badge-failed" title="${escapeHtml(reason)}"><i class="fa-solid fa-circle-exclamation"></i> Failed</span>`;
                title = escapeHtml(reason);
            } else {
                statusBadge = '<span class="badge-status badge-synced"><i class="fa-solid fa-circle-check"></i> Running</span>';
            }
        } else if (v.enabled) {
            statusBadge = '<span class="badge-status badge-stopped"><i class="fa-solid fa-circle-pause"></i> Stopped</span>';
        } else {
            statusBadge = '<span class="badge-status badge-disabled"><i class="fa-solid fa-ban"></i> Disabled</span>';
            rowClass = ' class="volume-disabled"';
        }
        const rt = v.runtime || {};
        const drive = (!v.invalid && v.running && rt.drive_letter) || '-';
        const pending = (!v.invalid && v.running && typeof rt.pending === "number") ? rt.pending : '-';
        const size = (!v.invalid && v.running && typeof rt.total_bytes === "number") ? formatBytes(rt.total_bytes) : '—';
        return `
            <tr${rowClass} title="${title}">
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
                <td>${volumeActionButtons(v)}</td>
            </tr>
        `;
    }).join("");
}

// The Actions cell (P1+P6): a running volume offers [Refresh] (rebuild
// its index from the remote backend — or the muted note where the
// backend cannot be rebuilt), [Unmount] (runtime removal — the file
// stays) and [Disable] (file + unmount); a stopped or disabled volume
// offers [Enable]. An invalid file offers nothing — it needs a hand
// edit first. Edit/Delete land in later batches.
function volumeActionButtons(v) {
    if (v.invalid) return '';
    const name = escapeHtml(v.name);
    if (v.running) {
        return `
            ${refreshControl(v, name)}
            <button class="btn-mini" data-action="unmount" data-name="${name}"
                    title="Unmount and unregister now (the volume file stays on disk)">
                <i class="fa-solid fa-eject"></i> Unmount
            </button>
            <button class="btn-mini btn-mini-danger" data-action="disable" data-name="${name}"
                    title="Write enabled = false to the volume file and unmount it (survives restarts)">
                <i class="fa-solid fa-power-off"></i> Disable
            </button>
        `;
    }
    return `
        <button class="btn-mini" data-action="enable" data-name="${name}"
                title="Write enabled = true and assemble the volume now">
            <i class="fa-solid fa-play"></i> Enable
        </button>
    `;
}

// The Refresh control (P6): a button on every running volume whose
// index the backend can rebuild (plaintext local/baidu), a muted note
// where it cannot — telegram's db IS its index (the shadow index; the
// guidance is `cydrive sync`) and an encrypted volume's backend only
// sees ciphertext containers (K11; the guidance is `cydrive sync`, the
// sync payload carries the encrypted row semantics). While the row's
// `rebuilding` marker is up (the poll's live state), the button renders
// in its busy shape — disabled, spinning — until the background pass
// settles.
function refreshControl(v, name) {
    if (v.backend === 'telegram') {
        return `<span class="volume-note" data-note="refresh-unsupported" title="Refresh is not supported for the telegram backend — its db IS the index (the shadow index); use \`cydrive sync\` to replicate it to another instance instead"><i class="fa-solid fa-rotate"></i></span>`;
    }
    if (v.encrypted) {
        return `<span class="volume-note" data-note="refresh-unsupported" title="Refresh refuses encrypted instances — the backend only sees ciphertext containers; use \`cydrive sync\` instead (the sync payload carries the encrypted row semantics)"><i class="fa-solid fa-rotate"></i></span>`;
    }
    const busy = v.rebuilding === true;
    return `
        <button class="btn-mini" data-action="refresh" data-name="${name}"${busy ? ' disabled' : ''}
                title="Rebuild this volume's index from its remote backend (idempotent, runs in the background)">
            <i class="fa-solid ${busy ? 'fa-circle-notch fa-spin' : 'fa-rotate'}"></i> Refresh
        </button>
    `;
}

// Event delegation: one listener on the tbody serves every rendered row.
function onVolumeActionClick(event) {
    const button = event.target.closest('button[data-action]');
    if (!button) return;
    runVolumeAction(button.dataset.action, button.dataset.name, button);
}

async function runVolumeAction(action, name, button) {
    const spec = VOLUME_ACTIONS[action];
    if (!spec) return;
    if (spec.confirm) {
        const message = action === 'unmount'
            ? `Unmount volume "${name}"?\nIts drive disappears immediately; the volume file stays on disk (re-enable anytime).`
            : `Disable volume "${name}"?\nThis writes enabled = false to its file (it stays disabled across restarts) and unmounts it.`;
        if (!confirm(message)) return;
    }
    setButtonLoading(button, true);
    try {
        const res = await fetch(`/api/volumes/${encodeURIComponent(name)}/${spec.route}`, {
            method: "POST",
        });
        const body = await res.json().catch(() => ({}));
        if (res.ok && body.ok) {
            showToast('ok', body.reply || 'Done.');
        } else {
            // The backend's ERR text is already actionable — show it
            // verbatim rather than replacing it with a generic message.
            showToast('err', body.error || `HTTP ${res.status}`);
        }
    } catch (err) {
        showToast('err', `request failed: ${err}`);
    } finally {
        // The refresh rebuilds the table (and the button with it); the
        // loading state only needs to span the request itself.
        setButtonLoading(button, false);
        await loadVolumesPage();
    }
}

function setButtonLoading(button, loading) {
    button.disabled = loading;
    const icon = button.querySelector('i');
    if (!icon) return;
    icon.className = loading
        ? 'fa-solid fa-circle-notch fa-spin'
        : ({
            unmount: 'fa-solid fa-eject',
            disable: 'fa-solid fa-power-off',
            enable: 'fa-solid fa-play',
            refresh: 'fa-solid fa-rotate',
        }[button.dataset.action] || 'fa-solid fa-circle');
}

// The toast host: created on demand, one slot per message, 4s self-destruct.
function showToast(kind, text) {
    let host = document.getElementById('toast-host');
    if (!host) {
        host = document.createElement('div');
        host.id = 'toast-host';
        document.body.appendChild(host);
    }
    const toast = document.createElement('div');
    toast.className = `toast toast-${kind}`;
    toast.innerHTML = `
        <i class="fa-solid ${kind === 'ok' ? 'fa-circle-check' : 'fa-circle-exclamation'}"></i>
        <span>${escapeHtml(text)}</span>
    `;
    host.appendChild(toast);
    setTimeout(() => {
        toast.classList.add('toast-out');
        setTimeout(() => toast.remove(), 350);
    }, 4000);
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
