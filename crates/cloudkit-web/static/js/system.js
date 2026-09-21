// The read-only system-parameters page (UI polish round 2, /volumes/system):
// every byte on this page comes from the EXISTING read endpoints —
// GET /api/volumes/configs (the volume-file full set), GET
// /api/volumes/{name}/config (the SHOW-masked per-volume config:
// explicit plain keys verbatim, credential keys as {"set": bool}) and
// GET /api/volumes (the runtime rows). There are NO write controls: the
// page renders, it never mutates.
//
// Section 1 fills the instance faces (dashboard address, the deduped
// WebDAV endpoint list, volume-file and running counts). Section 2
// renders one collapsible <details> card per volume file — the header
// carries name + backend + status, the expanded body a key/value table
// whose labels come from the syskey.* i18n map (an unmapped key renders
// under its raw toml name); a credential row shows only the
// set/unset badge — the value never left the backend (the SHOW
// write-only contract). A disabled volume's card greys out.
//
// No polling: a SHOW is a rare, human-scale command (the control seam's
// own words) and N per-volume fetches every 4s would enqueue behind
// real commands — the topbar refresh re-runs this whole load instead.

// Configuration-key → i18n label map (the keys the page knows how to
// name; anything else keeps its raw toml spelling).
const SYS_KEY_LABELS = {
    enabled: 'syskey.enabled',
    drive_letter: 'syskey.drive_letter',
    chunk_size_mb: 'syskey.chunk_size_mb',
    enable_encryption: 'syskey.enable_encryption',
    encryption_scheme: 'syskey.encryption_scheme',
    baidu_root: 'syskey.baidu_root',
    baidu_app_key: 'syskey.baidu_app_key',
    local_root: 'syskey.local_root',
    storage_path: 'syskey.storage_path',
    cache_path: 'syskey.cache_path',
    db_path: 'syskey.db_path',
    sync_url: 'syskey.sync_url',
    sync_interval_secs: 'syskey.sync_interval_secs',
    chat_id: 'syskey.chat_id',
    bot_token: 'syskey.bot_token',
    encryption_password: 'syskey.encryption_password',
    sync_secret: 'syskey.sync_secret',
    baidu_app_secret: 'syskey.baidu_app_secret',
    baidu_access_token: 'syskey.baidu_access_token',
    baidu_refresh_token: 'syskey.baidu_refresh_token',
};

// The keys the card header already shows — never repeated as table rows.
const SYS_HEADER_KEYS = { name: true, backend: true };

// Backend display names through the i18n dictionary (i18n.js loads
// first) — the volumes.js twin.
const VOLUME_LABELS = {
    telegram: 'backend.telegram',
    baidu: 'backend.baidu',
    local: 'backend.local',
    sftp: 'backend.sftp',
    pan115: 'backend.pan115',
    pan123: 'backend.pan123',
};

function backendDisplayName(backend) {
    const key = VOLUME_LABELS[backend];
    return key ? t(key) : (backend || t('common.unknown'));
}

document.addEventListener("DOMContentLoaded", () => {
    loadSystemPage();
});

async function loadSystemPage() {
    try {
        // allSettled (the volumes-page shape): a read-only boot without
        // the command seam still fills the instance faces off the
        // runtime rows — the per-volume configs fetch 503s alone.
        const [configsRes, runtimeRes] = await Promise.allSettled([
            fetch("/api/volumes/configs"),
            fetch("/api/volumes"),
        ]);
        const configs = configsRes.status === 'fulfilled' && configsRes.value.ok
            ? await configsRes.value.json()
            : null;
        const runtime = runtimeRes.status === 'fulfilled' && runtimeRes.value.ok
            ? await runtimeRes.value.json()
            : [];
        renderInstanceSection(configs, runtime);
        await renderVolumeCards(configs);
    } catch (err) {
        console.error("Error loading system parameters:", err);
    }
}

// The authority part of a webdav_url: `http://host:port/vol/X` →
// `host:port` (a portless URL keeps the authority as-is).
function webdavEndpoint(url) {
    const rest = String(url || '').replace(/^https?:\/\//, '');
    return rest.split('/')[0] || '';
}

// Section 1: the dashboard address is where the page itself lives; the
// endpoint list dedups the runtime rows' host:port parts in first-seen
// order; the file count is the configs FULL set (honestly `—` when the
// configs endpoint is unavailable — the runtime registry is NOT the
// file set), the running count reads the runtime rows.
function renderInstanceSection(configs, runtime) {
    setText('sys-dashboard-url', location.origin);
    const endpoints = [...new Set(runtime.map(v => webdavEndpoint(v.webdav_url)).filter(Boolean))];
    setText('sys-webdav-endpoints', endpoints.join(', ') || '—');
    setText('sys-volume-files', configs ? String(configs.length) : '—');
    setText('sys-running', String(runtime.filter(v => v.status === 'running').length));
}

// Section 2: one collapsible card per volume FILE (the configs listing —
// disabled and invalid files included; that is the point). Without the
// configs endpoint the section says so instead of showing a partial
// picture; the per-volume SHOW fetches run concurrently (they queue
// behind the same serialized command seam either way).
async function renderVolumeCards(configs) {
    const host = document.getElementById('sys-volume-cards');
    if (!host) return;
    if (!configs) {
        host.innerHTML = `
            <p class="sys-note">
                <i class="fa-solid fa-triangle-exclamation"></i>
                ${t('volumes.loading_error', { status: 503 })}
            </p>
        `;
        return;
    }
    if (!configs.length) {
        host.innerHTML = `
            <p class="sys-note"><i class="fa-solid fa-database"></i> ${t('volumes.empty')}</p>
        `;
        return;
    }
    const cards = await Promise.all(configs.map(sysCard));
    host.innerHTML = '';
    for (const card of cards) host.appendChild(card);
}

// One card: <details class="sys-card"> whose summary is the header row
// (name + backend + status badge) and whose body is either the
// key/value table or a muted "unavailable" note when that volume's
// SHOW fetch fails. enabled=false greys the whole card.
async function sysCard(row) {
    const details = document.createElement('details');
    details.className = 'sys-card' + (row.enabled === false ? ' sys-disabled' : '');
    const summary = document.createElement('summary');
    summary.innerHTML = `
        <span class="sys-card-name"><i class="fa-solid fa-database"></i> ${escapeHtml(row.name)}</span>
        <span class="sys-card-backend">${escapeHtml(backendDisplayName(row.backend))}</span>
        ${sysStatusBadge(row)}
    `;
    details.appendChild(summary);

    const body = document.createElement('div');
    body.className = 'sys-card-body';
    let config = null;
    try {
        const res = await fetch(`/api/volumes/${encodeURIComponent(row.name)}/config`);
        if (res.ok) config = await res.json();
    } catch (err) {
        config = null;
    }
    body.innerHTML = config ? kvTable(config) : `
        <p class="sys-note"><i class="fa-solid fa-circle-info"></i> ${t('sys.config_unavailable')}</p>
    `;
    details.appendChild(body);
    return details;
}

// The header's status badge — the volumes-table semantics, read-only
// shape: invalid (parse failure, reason in the tooltip), running,
// stopped (enabled but not assembled), disabled (enabled = false).
function sysStatusBadge(row) {
    if (row.invalid === true) {
        const reason = row.reason || '';
        return `<span class="badge-status badge-failed" title="${escapeHtml(reason)}"><i class="fa-solid fa-file-circle-xmark"></i> ${t('volumes.status.invalid')}</span>`;
    }
    if (row.enabled !== false && row.running === true) {
        return `<span class="badge-status badge-synced"><i class="fa-solid fa-circle-check"></i> ${t('volumes.status.running')}</span>`;
    }
    if (row.enabled !== false) {
        return `<span class="badge-status badge-stopped"><i class="fa-solid fa-circle-pause"></i> ${t('volumes.status.stopped')}</span>`;
    }
    return `<span class="badge-status badge-disabled"><i class="fa-solid fa-ban"></i> ${t('volumes.status.disabled')}</span>`;
}

// The expanded card's key/value table: every explicit key of the file
// (the SHOW reply) minus the header-duplicated ones, in file order —
// the i18n label when the key is mapped, the raw key when not. A
// credential's {"set": bool} marker renders as the set/unset badge (the
// value is not in the response); booleans render as yes/no.
function kvTable(config) {
    const rows = Object.entries(config)
        .filter(([key]) => !SYS_HEADER_KEYS[key])
        .map(([key, value]) => {
            const label = SYS_KEY_LABELS[key] ? t(SYS_KEY_LABELS[key]) : escapeHtml(key);
            return `
                <tr>
                    <th scope="row">${label}</th>
                    <td>${sysValueHtml(value)}</td>
                </tr>
            `;
        }).join("");
    return `<div class="table-responsive"><table class="files-table sys-kv"><tbody>${rows}</tbody></table></div>`;
}

function sysValueHtml(value) {
    // The SHOW write-only credential marker: {"set": bool}, never a value.
    if (value && typeof value === 'object' && typeof value.set === 'boolean') {
        return value.set
            ? `<span class="badge-status badge-synced"><i class="fa-solid fa-circle-check"></i> ${t('sys.cred_set')}</span>`
            : `<span class="badge-status badge-disabled"><i class="fa-solid fa-ban"></i> ${t('sys.cred_unset')}</span>`;
    }
    if (typeof value === 'boolean') {
        return t(value ? 'sys.value.yes' : 'sys.value.no');
    }
    if (value === null || value === undefined) return '—';
    return escapeHtml(String(value));
}

function setText(id, text) {
    const el = document.getElementById(id);
    if (el) el.innerText = text;
}

// K58 follow-up (external review): quotes are escaped too — the
// invalid reason (sysStatusBadge's title) and SHOW values land inside
// double-quoted attributes, where an unescaped quote would break out.
// & stays first: no double-escaping.
function escapeHtml(text) {
    return String(text).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
