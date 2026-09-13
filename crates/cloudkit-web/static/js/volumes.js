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
// P3 adds the volume form card (Add Volume): the backend radio swaps
// the credential group, the advanced section's empty fields write no
// keys (the backend's defaults rule), the form's checks are UX-level
// only (slug shape + the starred requireds) — the CREATE command is the
// authority and its ERR text renders in the form's red box with the
// values kept. P4 adds the Edit face on the same card: [Edit] pulls
// the volume's SHOW config (plain fields verbatim; the write-only
// credential keys show set/unset placeholders — an empty input keeps
// the stored value), the name locks, the notice spells the REMOVE+ADD
// re-assembly + comments-loss semantics, and a pending-upload count
// (the poll's live data) confirms before the save.

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
    initVolumeForm();
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
    // The last poll's merged rows — the edit flow's pending-upload
    // confirm reads its live data from here.
    vfLastRows = rows;
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

// The Actions cell (P1+P6, Edit since P4): a running volume offers
// [Refresh] (rebuild its index from the remote backend — or the muted
// note where the backend cannot be rebuilt), [Edit] (the edit form,
// prefilled from SHOW), [Unmount] (runtime removal — the file stays)
// and [Disable] (file + unmount); a stopped or disabled volume offers
// [Edit] + [Enable]. An invalid file offers nothing but the Delete
// placeholder — it needs a hand edit first (SHOW cannot prefill from a
// file the loader refuses). Delete is the P5 placeholder (two-step
// confirmation lands there).
function volumeActionButtons(v) {
    if (v.invalid) {
        return deletePlaceholder(v.name);
    }
    const name = escapeHtml(v.name);
    if (v.running) {
        return `
            ${refreshControl(v, name)}
            <button class="btn-mini" data-action="edit" data-name="${name}"
                    title="Edit this volume's configuration (prefilled from its file; saving re-assembles it)">
                <i class="fa-solid fa-pen"></i> Edit
            </button>
            <button class="btn-mini" data-action="unmount" data-name="${name}"
                    title="Unmount and unregister now (the volume file stays on disk)">
                <i class="fa-solid fa-eject"></i> Unmount
            </button>
            <button class="btn-mini btn-mini-danger" data-action="disable" data-name="${name}"
                    title="Write enabled = false to the volume file and unmount it (survives restarts)">
                <i class="fa-solid fa-power-off"></i> Disable
            </button>
            ${deletePlaceholder(v.name)}
        `;
    }
    return `
        <button class="btn-mini" data-action="edit" data-name="${name}"
                title="Edit this volume's configuration (prefilled from its file)">
            <i class="fa-solid fa-pen"></i> Edit
        </button>
        <button class="btn-mini" data-action="enable" data-name="${name}"
                title="Write enabled = true and assemble the volume now">
            <i class="fa-solid fa-play"></i> Enable
        </button>
        ${deletePlaceholder(v.name)}
    `;
}

// The P5 Delete placeholder (kept visible, disabled — the two-step
// confirmation modal lands in that batch).
function deletePlaceholder(name) {
    return `
        <button class="btn-mini btn-mini-danger" data-action="delete" data-name="${escapeHtml(name)}"
                disabled title="coming in P5">
            <i class="fa-solid fa-trash"></i> Delete
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
    // Edit is a UI action (the edit form), not a command POST.
    if (button.dataset.action === 'edit') {
        openEditVolumeForm(button.dataset.name);
        return;
    }
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

// ------------------------- the volume form card (web volume mgmt P3) -----

// The backend radio → credential group map.
const VF_CRED_GROUPS = {
    telegram: 'vf-group-telegram',
    baidu: 'vf-group-baidu',
    local: 'vf-group-local',
};

// The starred requireds per backend (UX-level only — the CREATE command
// is the authority and its ERR text renders in the red box).
const VF_REQUIRED = {
    telegram: ['vf-bot-token', 'vf-chat-id'],
    baidu: ['vf-baidu-app-key', 'vf-baidu-app-secret', 'vf-baidu-access-token', 'vf-baidu-refresh-token'],
    local: ['vf-local-root'],
};

// String fields: input id → payload key (collected only when non-empty —
// an empty string writes no key, the backend's default rules).
const VF_STRINGS = {
    'vf-drive': 'drive_letter',
    'vf-bot-token': 'bot_token',
    'vf-baidu-app-key': 'baidu_app_key',
    'vf-baidu-app-secret': 'baidu_app_secret',
    'vf-baidu-access-token': 'baidu_access_token',
    'vf-baidu-refresh-token': 'baidu_refresh_token',
    'vf-baidu-root': 'baidu_root',
    'vf-local-root': 'local_root',
    'vf-enc-pass': 'encryption_password',
    'vf-sync-url': 'sync_url',
    'vf-sync-secret': 'sync_secret',
};

// Numeric fields: input id → payload key (collected only when non-empty
// and whole-number shaped).
const VF_NUMBERS = {
    'vf-chat-id': 'chat_id',
    'vf-chunk': 'chunk_size_mb',
    'vf-sync-interval': 'sync_interval_secs',
};

// The form card's current mode: null (closed), 'create' or 'edit'.
let vfMode = null;

// The last poll's merged rows (the Edit flow's pending-upload confirm
// reads its live data from here — assigned in renderVolumesTable).
let vfLastRows = [];

function vfEl(id) {
    return document.getElementById(id);
}

function vfValue(id) {
    const el = vfEl(id);
    return el ? el.value.trim() : '';
}

// Wires the form card's static listeners (idempotent — the harness and
// the DOMContentLoaded path both call it).
function initVolumeForm() {
    const addBtn = vfEl('add-volume-btn');
    if (addBtn && !addBtn.dataset.wired) {
        addBtn.dataset.wired = '1';
        addBtn.addEventListener('click', () => openVolumeForm('create'));
    }
    const cancel = vfEl('volume-form-cancel');
    if (cancel && !cancel.dataset.wired) {
        cancel.dataset.wired = '1';
        cancel.addEventListener('click', closeVolumeForm);
    }
    const form = vfEl('volume-form');
    if (form && !form.dataset.wired) {
        form.dataset.wired = '1';
        form.addEventListener('submit', onVolumeFormSubmit);
    }
    const name = vfEl('vf-name');
    if (name && !name.dataset.wired) {
        name.dataset.wired = '1';
        name.addEventListener('input', vfCheckName);
    }
    const backend = vfEl('vf-backend');
    if (backend && !backend.dataset.wired) {
        backend.dataset.wired = '1';
        backend.addEventListener('change', vfSwapCredentialGroups);
    }
    // The /volumes#add anchor opens the form (the usage-state page's ＋
    // chip target — the P0 hook).
    if (vfMode === null && location.hash === '#add') openVolumeForm('create');
}

// Opens the card in the given mode: 'create' resets everything; 'edit'
// resets then prefills from the SHOW config (the name locks — it is the
// volume's identity).
function openVolumeForm(mode, preset) {
    vfMode = mode;
    const card = vfEl('volume-form-card');
    if (!card) return;
    card.hidden = false;
    vfHideError();
    const title = vfEl('volume-form-title');
    const submitLabel = vfEl('vf-submit-label');
    const notice = vfEl('volume-form-notice');
    const nameInput = vfEl('vf-name');
    if (mode === 'create') {
        if (title) title.innerHTML = '<i class="fa-solid fa-plus"></i> Add Volume';
        if (submitLabel) submitLabel.innerText = 'Create volume';
        if (notice) notice.hidden = true;
        resetVolumeForm();
    } else if (mode === 'edit') {
        if (title) title.innerHTML = '<i class="fa-solid fa-pen"></i> Edit Volume';
        if (submitLabel) submitLabel.innerText = 'Save changes';
        if (notice) {
            notice.innerText = 'Saving re-assembles the volume (REMOVE + ADD): the drive \
briefly disappears and pending uploads must drain first. The volume file is rewritten — \
hand-written comments are lost. Credential fields left empty keep their stored values.';
            notice.hidden = false;
        }
        resetVolumeForm();
        vfPrefillEdit(preset);
    }
    if (nameInput) nameInput.focus();
    card.scrollIntoView({ behavior: 'smooth', block: 'start' });
}

function closeVolumeForm() {
    vfMode = null;
    const card = vfEl('volume-form-card');
    if (card) card.hidden = true;
}

function resetVolumeForm() {
    const form = vfEl('volume-form');
    if (form) form.reset();
    // reset() leaves the DOM's checked/default state; force the dynamic
    // pieces explicitly so a previous open never bleeds through.
    const enabled = vfEl('vf-enabled');
    if (enabled) enabled.checked = true;
    const scheme = vfEl('vf-enc-scheme');
    if (scheme) scheme.value = 'gcm';
    const name = vfEl('vf-name');
    if (name) {
        name.readOnly = false;
        vfCheckName();
    }
    // Restore the create-mode placeholders the edit prefill replaced
    // with set/unset markers.
    document.querySelectorAll('.vf-cred-group input, #vf-advanced input').forEach(input => {
        if (input.dataset.createPlaceholder !== undefined) {
            input.placeholder = input.dataset.createPlaceholder;
            delete input.dataset.createPlaceholder;
        }
    });
    vfSwapCredentialGroups();
    vfHideError();
}

// The SHOW prefill for the edit mode (P4): plain values verbatim, the
// write-only credential keys showing their set/unset placeholders (the
// value never left the backend — an empty input keeps the stored one).
function vfPrefillEdit(config) {
    if (!config) return;
    const name = vfEl('vf-name');
    if (name) {
        name.value = config.name || '';
        name.readOnly = true;
    }
    const plain = {
        'vf-drive': config.drive_letter,
        'vf-chat-id': config.chat_id,
        'vf-baidu-root': config.baidu_root,
        'vf-local-root': config.local_root,
        'vf-sync-url': config.sync_url,
    };
    for (const [id, value] of Object.entries(plain)) {
        const el = vfEl(id);
        if (el && value !== undefined && value !== null) el.value = value;
    }
    // The write-only credential keys: SHOW answered {"set": bool}, never
    // a value — the placeholder states whether one is stored.
    const writeOnly = {
        'vf-bot-token': config.bot_token,
        'vf-baidu-app-key': config.baidu_app_key,
        'vf-baidu-app-secret': config.baidu_app_secret,
        'vf-baidu-access-token': config.baidu_access_token,
        'vf-baidu-refresh-token': config.baidu_refresh_token,
        'vf-enc-pass': config.encryption_password,
        'vf-sync-secret': config.sync_secret,
    };
    for (const [id, marker] of Object.entries(writeOnly)) {
        const el = vfEl(id);
        if (!el) continue;
        el.dataset.createPlaceholder = el.placeholder;
        el.value = '';
        el.placeholder = marker && marker.set
            ? '已设置（留空 = 不修改）'
            : '未设置';
    }
    // The radio + checkbox faces (an absent enabled key reads enabled —
    // the natural default).
    const backend = config.backend || 'telegram';
    const radio = document.querySelector(`input[name="vf-backend"][value="${backend}"]`);
    if (radio) radio.checked = true;
    const enc = vfEl('vf-enc');
    if (enc) enc.checked = config.enable_encryption === true;
    const scheme = vfEl('vf-enc-scheme');
    if (scheme) scheme.value = config.encryption_scheme === 'aead_v2' ? 'aead_v2' : 'gcm';
    const chunk = vfEl('vf-chunk');
    if (chunk && typeof config.chunk_size_mb === 'number') chunk.value = config.chunk_size_mb;
    const interval = vfEl('vf-sync-interval');
    if (interval && typeof config.sync_interval_secs === 'number') {
        interval.value = config.sync_interval_secs;
    }
    const enabled = vfEl('vf-enabled');
    if (enabled) enabled.checked = config.enabled !== false;
    vfSwapCredentialGroups();
}

// The [Edit] entry point: pull the volume's SHOW config and open the
// card in edit mode (a broken SHOW toasts its actionable error and
// leaves the table alone).
async function openEditVolumeForm(name) {
    try {
        const res = await fetch(`/api/volumes/${encodeURIComponent(name)}/config`);
        if (!res.ok) {
            const body = await res.json().catch(() => ({}));
            showToast('err', body.error || `HTTP ${res.status}`);
            return;
        }
        const config = await res.json();
        openVolumeForm('edit', config);
    } catch (err) {
        showToast('err', `request failed: ${err}`);
    }
}

// Live slug check (UX-level; the CREATE command re-validates with the
// same rule server-side).
const VF_NAME_RULE = /^[a-z][a-z0-9_-]{0,31}$/;

function vfCheckName() {
    const input = vfEl('vf-name');
    const hint = vfEl('vf-name-hint');
    if (!input || !hint) return;
    const value = input.value.trim();
    const bad = value !== '' && !VF_NAME_RULE.test(value);
    input.classList.toggle('vf-invalid', bad);
    hint.classList.toggle('vf-hint-bad', bad);
    if (bad) hint.innerText = 'names start with a lowercase letter; only a-z, 0-9, _ and -; up to 32 chars';
    else hint.innerText = 'a–z, 0–9, _ and - ; starts with a letter, up to 32 chars';
}

function vfSwapCredentialGroups() {
    const backend = vfBackend();
    for (const [name, id] of Object.entries(VF_CRED_GROUPS)) {
        const group = vfEl(id);
        if (group) group.hidden = name !== backend;
    }
}

function vfBackend() {
    const radio = document.querySelector('input[name="vf-backend"]:checked');
    return radio ? radio.value : 'telegram';
}

function vfShowError(text) {
    const box = vfEl('vf-error');
    if (!box) return;
    box.innerText = text;
    box.hidden = false;
}

function vfHideError() {
    const box = vfEl('vf-error');
    if (box) box.hidden = true;
}

// UX-level validation: the slug shape and (in CREATE mode) the starred
// requireds of the chosen backend — in EDIT mode an empty credential
// field means "keep the stored value", so the stars gate creation only
// (the CREATE/UPDATE command is the authority either way; everything
// else rides its ERR text into the red box).
function vfValidate() {
    const name = vfValue('vf-name');
    if (!VF_NAME_RULE.test(name)) {
        return 'Pick a volume name first: a lowercase letter, then a-z/0-9/_/- , up to 32 characters.';
    }
    if (vfMode === 'create') {
        for (const id of VF_REQUIRED[vfBackend()] || []) {
            if (!vfValue(id)) {
                const el = vfEl(id);
                const label = el ? (el.closest('.volume-field')?.querySelector('span')?.innerText || id) : id;
                return `The ${label.trim().replace('*', '')} field is required for this backend.`;
            }
        }
    }
    for (const id of Object.keys(VF_NUMBERS)) {
        const raw = vfValue(id);
        if (raw !== '' && !/^-?\d+$/.test(raw)) {
            return `"${raw}" is not a whole number — the ${VF_NUMBERS[id]} field takes digits only.`;
        }
    }
    return '';
}

// Collects the payload object (every key the form sets; empty strings
// and unset numbers are OMITTED — they would write no key server-side).
function vfCollectPayload() {
    const payload = {};
    for (const [id, key] of Object.entries(VF_STRINGS)) {
        const value = vfValue(id);
        if (value !== '') payload[key] = value;
    }
    for (const [id, key] of Object.entries(VF_NUMBERS)) {
        const raw = vfValue(id);
        if (raw !== '') payload[key] = Number(raw);
    }
    const enabled = vfEl('vf-enabled');
    if (enabled) payload.enabled = enabled.checked;
    const enc = vfEl('vf-enc');
    if (enc) payload.enable_encryption = enc.checked;
    const scheme = vfEl('vf-enc-scheme');
    if (scheme && scheme.value !== 'gcm') payload.encryption_scheme = scheme.value;
    const backend = vfBackend();
    if (backend) payload.backend = backend;
    return payload;
}

async function onVolumeFormSubmit(event) {
    event.preventDefault();
    if (!vfMode) return;
    vfHideError();
    const problem = vfValidate();
    if (problem) {
        vfShowError(problem);
        return;
    }
    // The edit-mode re-assembly takes the volume down (REMOVE + ADD) —
    // pending uploads would be drained (waited on) or abort the update;
    // the poll's live pending count is the confirm's data.
    if (vfMode === 'edit') {
        const name = vfValue('vf-name');
        const row = vfLastRows.find(r => r.name === name);
        const pending = row && row.runtime && typeof row.runtime.pending === "number"
            ? row.runtime.pending : 0;
        if (pending > 0) {
            const proceed = confirm(
                `Volume "${name}" has ${pending} pending upload(s).\n` +
                'Saving re-assembles the volume (REMOVE + ADD): the update waits for the ' +
                'queue to drain (or is refused while uploads keep arriving).\n\nProceed?'
            );
            if (!proceed) return;
        }
    }
    const submit = vfEl('vf-submit');
    const label = vfEl('vf-submit-label');
    const idleLabel = vfMode === 'create' ? 'Create volume' : 'Save changes';
    if (submit) submit.disabled = true;
    if (label) label.innerText = vfMode === 'create' ? 'Creating…' : 'Saving…';
    try {
        let response, body;
        if (vfMode === 'create') {
            const payload = vfCollectPayload();
            payload.name = vfValue('vf-name');
            response = await fetch('/api/volumes', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(payload),
            });
        } else {
            // The edit transport (P4): the name rides the path.
            const name = vfValue('vf-name');
            response = await fetch(`/api/volumes/${encodeURIComponent(name)}`, {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(vfCollectPayload()),
            });
        }
        body = await response.json().catch(() => ({}));
        if (response.ok && body.ok) {
            showToast('ok', body.reply || 'Done.');
            closeVolumeForm();
        } else {
            // The backend's ERR text is already actionable — keep the
            // form (and every entered value) and show it in the red box.
            vfShowError(body.error || `HTTP ${response.status}`);
        }
    } catch (err) {
        vfShowError(`request failed: ${err}`);
    } finally {
        if (submit) submit.disabled = false;
        if (label) label.innerText = idleLabel;
        await loadVolumesPage();
    }
}
