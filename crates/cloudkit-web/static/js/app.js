let allFiles = [];
let currentFilter = 'all';

// Client-side pagination of the files table: allFiles is fully in
// memory, so a page is just a slice of the current filter result. The
// page resets on user-driven filter changes (search box, view filters,
// the page-size select) and only clamps on poll refreshes — so a delete
// that empties the current page steps back instead of staring at a
// blank table. The page size is the user's own choice (the bar's
// select; persisted to localStorage, an unusable stored value falls
// back to the default).
const PAGE_SIZES = [10, 20, 50];
const DEFAULT_PAGE_SIZE = 10;
let currentPage = 1;
let filteredFiles = [];

// The stored page size, or the default when nothing (or something
// unusable — off-list, non-numeric) is stored. Storage may be disabled
// in hostile embedders; a failed read must never break the UI.
function readPageSize() {
    try {
        const stored = Number(localStorage.getItem('cydrive.pageSize'));
        if (PAGE_SIZES.includes(stored)) return stored;
    } catch (err) { /* storage off */ }
    return DEFAULT_PAGE_SIZE;
}
let pageSize = readPageSize();

// The active backend's identity, refreshed from every /api/stats poll
// (server-reported — the UI never guesses which backend is running).
let backendState = {
    backend: null,        // 'telegram' | 'baidu' | 'local' | ...
    remoteDelete: false,  // true: a delete removes the cloud object too
    configured: false,    // telegram-side credentials present
};

// Multi-volume mode state (Phase 2.5 / K24): inactive until the boot
// probe sees the 400-with-volumes reply that marks a registry
// dashboard. `current` names the selected volume; every volume-scoped
// fetch then carries `?volume=<name>` (K23 — the server has no default
// volume).
let volumeState = {
    multi: false,
    volumes: [],   // /api/volumes rows (name/backend/status/...)
    current: null, // selected volume name
};

// Display names per backend spelling (the /api/stats `backend` values),
// through the i18n dictionary (i18n.js loads first).
const BACKEND_LABELS = {
    telegram: 'backend.telegram',
    baidu: 'backend.baidu',
    local: 'backend.local',
};

function backendDisplayName(backend) {
    const key = BACKEND_LABELS[backend];
    return key ? t(key) : t('backend.fallback');
}

document.addEventListener("DOMContentLoaded", async () => {
    await detectMultiVolumeMode();
    loadDriveData();
    setupDropZone();
    setupSearch();
    setupPagination();
    // Auto-refresh drive files and stats every 4 seconds
    setInterval(loadDriveData, 4000);
});

// One delegated listener pair on the static pagination host serves
// every bar the renderer mints (clicks move pages, the select changes
// the page size — both survive the bar's re-renders).
function setupPagination() {
    const host = document.getElementById("pagination");
    if (!host) return;
    host.addEventListener("click", onPaginationClick);
    host.addEventListener("change", onPageSizeChange);
}

// Boot probe: a parameterless /api/stats answers 200 on a single-volume
// dashboard and 400 + the volume list on a registry dashboard (K23).
// The remembered selection (localStorage, web volume management §1.4)
// restores across the / ↔ /volumes page hops — only while the volume
// still runs; a stale memory falls back to the first running volume.
async function detectMultiVolumeMode() {
    try {
        const res = await fetch("/api/stats");
        if (res.status === 400) {
            const body = await res.json().catch(() => null);
            if (body && Array.isArray(body.volumes)) {
                volumeState.multi = true;
                await refreshVolumes();
                const remembered = recallVolume();
                const rememberedOk = remembered && volumeState.volumes.some(
                    v => v.name === remembered && v.status === 'running');
                const first = volumeState.volumes.find(v => v.status === 'running');
                volumeState.current = rememberedOk ? remembered : (first ? first.name : null);
                renderVolumeTabs();
            }
        }
    } catch (err) {
        console.error("Mode detection failed:", err);
    }
}

// One /api/volumes call carries everything the volume tabs need
// (server reads db metadata — no per-volume N+1 polling).
async function refreshVolumes() {
    try {
        const res = await fetch("/api/volumes");
        if (res.ok) {
            volumeState.volumes = await res.json();
        }
    } catch (err) {
        console.error("Error loading volumes:", err);
    }
}

// A volume-scoped API URL for the selected volume (single-volume mode
// and volume-less edge cases pass the path through untouched).
function apiUrl(path) {
    if (!volumeState.multi || !volumeState.current) return path;
    return path + (path.includes("?") ? "&" : "?") +
        "volume=" + encodeURIComponent(volumeState.current);
}

async function loadDriveData() {
    try {
        if (volumeState.multi) {
            const [statsRes, filesRes, volumesRes] = await Promise.all([
                fetch(apiUrl("/api/stats")),
                fetch(apiUrl("/api/files")),
                fetch("/api/volumes")
            ]);

            if (volumesRes.ok) {
                volumeState.volumes = await volumesRes.json();
                renderVolumeTabs();
            }

            if (statsRes.ok) {
                const stats = await statsRes.json();
                updateStatsUI(stats);
            }

            if (filesRes.ok) {
                allFiles = await filesRes.json();
                applyCurrentFilter();
            }
        } else {
            const [statsRes, filesRes] = await Promise.all([
                fetch("/api/stats"),
                fetch("/api/files")
            ]);

            if (statsRes.ok) {
                const stats = await statsRes.json();
                updateStatsUI(stats);
            }

            if (filesRes.ok) {
                allFiles = await filesRes.json();
                applyCurrentFilter();
            }
        }
    } catch (err) {
        console.error("Error loading drive data:", err);
    }
}

// The volume switcher: one chip per registry volume — name + backend
// badge + status dot. Failed volumes are disabled but visible, with
// the server-reported reason as the tooltip (K22: failures are shown,
// never silently dropped). The row-end ＋ links to the management page
// (web volume management §1.4; the #add anchor lands with the P3
// form). After every redraw the current selection is revalidated
// (ensureCurrentVolume).
function renderVolumeTabs() {
    const bar = document.getElementById("volume-tabs");
    if (!bar || !volumeState.multi) return;
    bar.hidden = false;
    const chips = volumeState.volumes.map(v => {
        const failed = v.status === 'failed';
        const active = v.name === volumeState.current ? " active" : "";
        const reason = failed && v.status_reason
            ? ` title="failed: ${escapeHtml(v.status_reason)}"` : "";
        const dot = failed
            ? '<span class="volume-dot dot-failed"></span>'
            : '<span class="volume-dot dot-running"></span>';
        const label = `${escapeHtml(v.name)} <span class="volume-backend">${escapeHtml(v.backend || "")}</span> ${dot}`;
        return failed
            ? `<button class="volume-tab failed" disabled${reason}>${label}</button>`
            : `<button class="volume-tab${active}" onclick="switchVolume('${escapeHtml(v.name)}')"${reason}>${label}</button>`;
    }).join("");
    bar.innerHTML = `${chips}<a href="/volumes" class="volume-tab volume-add" title="${t('index.manage_volumes')}">＋</a>`;
    ensureCurrentVolume();
}

// The current-selection fallback (plan §1.4 顺带修复): a volume that
// was REMOVE'd server-side leaves volumeState.current dangling — the
// tabs would render no active chip and every volume-scoped fetch would
// 404, freezing the UI. When the selection no longer names a registry
// volume, fall back to the first running one (the first entry when
// nothing runs), persist it, redraw the tabs and reload the data once.
function ensureCurrentVolume() {
    if (!volumeState.multi) return;
    const names = volumeState.volumes.map(v => v.name);
    if (volumeState.current && names.includes(volumeState.current)) return;
    const fallback = volumeState.volumes.find(v => v.status === 'running')
        || volumeState.volumes[0]
        || null;
    if (!fallback) return; // empty registry: nothing to fall back to
    volumeState.current = fallback.name;
    rememberVolume(volumeState.current);
    renderVolumeTabs();
    loadDriveData();
}

function switchVolume(name) {
    if (volumeState.current === name) return;
    volumeState.current = name;
    rememberVolume(name);
    renderVolumeTabs();
    loadDriveData();
}

// The remembered selection's persistence (localStorage may be disabled
// in hostile embedders — a failed write must never break the UI).
function rememberVolume(name) {
    try { localStorage.setItem('cydrive.volume', name); } catch (err) { /* storage off */ }
}

function recallVolume() {
    try { return localStorage.getItem('cydrive.volume'); } catch (err) { return null; }
}

function updateStatsUI(stats) {
    const totalFilesEl = document.getElementById("stat-total-files");
    if (totalFilesEl) totalFilesEl.innerText = stats.total_files || 0;

    const bytes = stats.total_bytes || 0;
    const mb = (bytes / (1024 * 1024)).toFixed(1);
    const gb = (bytes / (1024 * 1024 * 1024)).toFixed(2);
    const sizeStr = bytes > (1024 * 1024 * 1024) ? `${gb} GB` : `${mb} MB`;

    const sizeEl = document.getElementById("stat-total-size");
    if (sizeEl) sizeEl.innerText = sizeStr;

    // The backend identity drives every copy line below.
    backendState.backend = stats.backend || null;
    backendState.remoteDelete = stats.remote_delete === true;
    backendState.configured = stats.is_configured === true;

    updateStorageCard(stats, sizeStr);
    updateBackendUI(stats);

    if (stats.drive_letter) {
        const letterEl = document.getElementById("drive-letter");
        if (letterEl) letterEl.innerText = stats.drive_letter;
    }
}

// The storage card (a stats-grid stat-card since the sidebar rework):
// the quota percent is the headline, used/total the detail line, and
// the static drive badge carries the mount letter. No progress bar
// anymore — the percent headline took over its job.
//
// Local disk: no quota concept — the number is what the drive indexes.
// A real backend ceiling: the ratio is the true used/total share (boot
// snapshot from /api/stats — informational, not live). Unlimited
// (telegram) or the quota snapshot unavailable: there is no honest
// ratio, so the headline rests at ∞.
function updateStorageCard(stats, indexedStr) {
    const detailEl = document.getElementById("storage-detail");
    const percentEl = document.getElementById("storage-percent");
    if (!detailEl || !percentEl) return;

    if (backendState.backend === 'local') {
        detailEl.innerText = t('index.on_local_disk', { size: indexedStr });
        percentEl.innerText = '—';
        return;
    }

    if (Number.isFinite(stats.quota_total) && stats.quota_total > 0) {
        const used = stats.quota_used || 0;
        const pct = Math.min(100, Math.round((used / stats.quota_total) * 100));
        detailEl.innerText = `${formatBytes(used)} / ${formatBytes(stats.quota_total)}`;
        percentEl.innerText = `${pct}%`;
        return;
    }

    detailEl.innerText = t('index.size_unlimited', { size: indexedStr });
    percentEl.innerText = '∞';
}

function updateBackendUI(stats) {
    const label = backendDisplayName(backendState.backend);

    // Brand badge: the backend spelling, verbatim.
    const badge = document.getElementById("backend-badge");
    if (badge && backendState.backend) {
        badge.innerText = backendState.backend;
        badge.hidden = false;
    }

    // Sync stat card: the backend display name, with the identity
    // detail as a pill (telegram: the chat id once configured;
    // baidu/local: the dispatched volume).
    const labelEl = document.getElementById("backend-label");
    if (labelEl) labelEl.innerText = label;

    const detailEl = document.getElementById("backend-detail");
    if (detailEl) {
        let detail = '';
        if (backendState.backend === 'telegram') {
            if (backendState.configured && stats.chat_id) detail = `chat ${stats.chat_id}`;
        } else if (stats.volume) {
            detail = stats.volume;
        }
        detailEl.innerText = detail;
        detailEl.hidden = detail === '';
    }

    // WebDAV card: the actually bound URL from the server.
    const webdavEl = document.getElementById("webdav-endpoint");
    if (webdavEl && stats.webdav_url) webdavEl.innerText = stats.webdav_url;

    // Drop zone copy: name the real destination.
    const dropSub = document.getElementById("drop-zone-sub");
    if (dropSub && backendState.backend) {
        dropSub.innerText = t('index.drop_sub_backend', { backend: label });
    }
}

function filterType(type) {
    currentFilter = type;

    // Update active nav-item class
    document.querySelectorAll(".nav-menu .nav-item").forEach(item => {
        item.classList.remove("active");
    });

    const eventTarget = window.event ? window.event.currentTarget : null;
    if (eventTarget) {
        eventTarget.classList.add("active");
    }

    applyCurrentFilter(true);
}

// The current view's file set: the type filter crossed with the search
// box (both user-driven, so either resets the page).
function filterFiles() {
    let filtered = allFiles;

    if (currentFilter === 'media') {
        filtered = allFiles.filter(f => isMedia(f.name));
    } else if (currentFilter === 'documents') {
        filtered = allFiles.filter(f => isDocument(f.name));
    }

    const searchInput = document.getElementById("search-input");
    if (searchInput && searchInput.value.trim()) {
        const q = searchInput.value.toLowerCase().trim();
        filtered = filtered.filter(f => f.name.toLowerCase().includes(q));
    }
    return filtered;
}

// The one slice entry point every refresh path goes through: the poll,
// the search box, the view filters and the post-delete reload all end
// here. resetPage=true on the user-driven paths (fresh filter, first
// page); the poll path clamps only, so a live refresh never yanks the
// reader off their page — and a delete that emptied the current page
// steps back to the last page that has rows.
function applyCurrentFilter(resetPage) {
    if (resetPage) currentPage = 1;
    filteredFiles = filterFiles();
    const pages = Math.max(1, Math.ceil(filteredFiles.length / pageSize));
    if (currentPage > pages) currentPage = pages;

    const start = (currentPage - 1) * pageSize;
    renderFilesTable(filteredFiles.slice(start, start + pageSize));
    renderPagination();
}

// The pagination bar under the table: prev/next (disabled at the
// bounds), the x / y indicator, the total — and the per-page size
// select (10/20/50, i18n-labelled "n / page"), whose choice persists
// and re-slices from page 1. Hidden while there is nothing to page
// through (the empty-state row carries the table).
function renderPagination() {
    const host = document.getElementById("pagination");
    if (!host) return;
    const total = filteredFiles.length;
    if (!total) {
        host.hidden = true;
        host.innerHTML = "";
        return;
    }
    host.hidden = false;
    const pages = Math.ceil(total / pageSize);
    const options = PAGE_SIZES.map(n =>
        `<option value="${n}"${n === pageSize ? " selected" : ""}>${t('pagination.per_page', { n })}</option>`
    ).join("");
    host.innerHTML = `
        <button class="page-btn" data-page="${currentPage - 1}"${currentPage <= 1 ? " disabled" : ""}>
            <i class="fa-solid fa-chevron-left"></i> ${t('pagination.prev')}
        </button>
        <span class="page-info">${t('pagination.page_x_of_y', { x: currentPage, y: pages })}</span>
        <button class="page-btn" data-page="${currentPage + 1}"${currentPage >= pages ? " disabled" : ""}>
            ${t('pagination.next')} <i class="fa-solid fa-chevron-right"></i>
        </button>
        <span class="page-total">${t('pagination.total', { n: total })}</span>
        <label class="page-size">
            <select class="page-size-select" aria-label="${t('pagination.per_page', { n: pageSize })}">${options}</select>
        </label>
    `;
}

// The size select's delegated change handler: an unusable value is
// ignored (the select only offers the PAGE_SIZES list), a real change
// persists to localStorage and re-slices from page 1 — a size change is
// a user-driven view change, so it resets the page like the filters do.
function onPageSizeChange(event) {
    const select = event.target.closest("select.page-size-select");
    if (!select) return;
    const size = Number(select.value);
    if (!PAGE_SIZES.includes(size) || size === pageSize) return;
    pageSize = size;
    try { localStorage.setItem('cydrive.pageSize', String(size)); } catch (err) { /* storage off */ }
    applyCurrentFilter(true);
}

// The bar's click handler: one delegated listener moves by the data-page
// the buttons carry (one behind/one ahead of the current page).
function onPaginationClick(event) {
    const button = event.target.closest("button[data-page]");
    if (!button || button.disabled) return;
    const pages = Math.max(1, Math.ceil(filteredFiles.length / pageSize));
    const page = Math.min(pages, Math.max(1, Number(button.dataset.page)));
    if (page === currentPage) return;
    currentPage = page;
    const start = (currentPage - 1) * pageSize;
    renderFilesTable(filteredFiles.slice(start, start + pageSize));
    renderPagination();
    const table = document.querySelector(".files-table");
    if (table) table.scrollIntoView({ behavior: "smooth", block: "start" });
}

function renderFilesTable(files) {
    const tbody = document.getElementById("files-tbody");
    const countLabel = document.getElementById("file-count-label");
    // The count label reports the whole filtered set (the pagination
    // bar under the table breaks down the page math).
    if (countLabel) countLabel.innerText = `${filteredFiles.length} ${t('index.items')}`;

    if (!files || files.length === 0) {
        const target = backendState.backend
            ? backendDisplayName(backendState.backend)
            : t('backend.fallback');
        tbody.innerHTML = `
            <tr>
                <td colspan="5" style="text-align: center; color: var(--text-muted); padding: 3rem;">
                    <i class="fa-solid fa-folder-open" style="font-size: 2.2rem; margin-bottom: 0.8rem; display: block; color: var(--accent-cyan); opacity: 0.6;"></i>
                    ${t('index.empty_files', { target: escapeHtml(target) })}
                </td>
            </tr>
        `;
        return;
    }

    tbody.innerHTML = files.map(file => {
        const icon = getFileIcon(file.name, file.is_dir);
        const sizeStr = file.is_dir ? "-" : formatBytes(file.size);
        const dateStr = file.mtime ? new Date(file.mtime * 1000).toLocaleDateString() : "-";
        const statusBadge = file.is_uploaded
            ? `<span class="badge-status badge-synced"><i class="fa-solid fa-circle-check"></i> ${t('status.synced')}</span>`
            : `<span class="badge-status badge-uploading"><i class="fa-solid fa-rotate fa-spin"></i> ${t('status.syncing')}</span>`;

        const isDir = Boolean(file.is_dir);
        const encName = encodeURIComponent(file.name);
        // The delete semantics are server-declared (K4): a remote-delete
        // backend really removes the cloud object, telegram only drops
        // the local row.
        const deleteTitle = backendState.remoteDelete
            ? t('index.delete_remote')
            : t('index.delete_local');

        return `
            <tr>
                <td>
                    <div class="file-name-cell">
                        ${icon}
                        <span title="${escapeHtml(file.name)}">${escapeHtml(file.name)}</span>
                    </div>
                </td>
                <td>${sizeStr}</td>
                <td>${statusBadge}</td>
                <td>${dateStr}</td>
                <td>
                    ${!isDir ? `<button class="action-btn" title="${t('index.copy_link')}" onclick="copyLink('${encName}')"><i class="fa-solid fa-link"></i></button>` : ''}
                    ${!isDir ? `<a href="${apiUrl(`/api/download/${encName}`)}" class="action-btn" title="${t('index.download')}"><i class="fa-solid fa-download"></i></a>` : ''}
                    ${!isDir && isMedia(file.name) ? `<button class="action-btn" title="${t('index.stream')}" onclick="previewMedia('${encName}')"><i class="fa-solid fa-play"></i></button>` : ''}
                    <button class="action-btn btn-delete" title="${deleteTitle}" onclick="deleteFile('${encName}')"><i class="fa-solid fa-trash-can"></i></button>
                </td>
            </tr>
        `;
    }).join("");
}

function getFileIcon(name, isDir) {
    if (isDir) return '<i class="fa-solid fa-folder" style="color: #ffd166; font-size: 1.2rem;"></i>';
    const ext = name.split('.').pop().toLowerCase();
    
    if (['jpg', 'jpeg', 'png', 'gif', 'webp', 'svg', 'bmp'].includes(ext)) {
        return '<i class="fa-solid fa-file-image" style="color: #00f3ff; font-size: 1.2rem;"></i>';
    } else if (['mp4', 'mkv', 'avi', 'mov', 'webm'].includes(ext)) {
        return '<i class="fa-solid fa-file-video" style="color: #ff007f; font-size: 1.2rem;"></i>';
    } else if (['mp3', 'wav', 'flac', 'ogg', 'm4a'].includes(ext)) {
        return '<i class="fa-solid fa-file-audio" style="color: #c084fc; font-size: 1.2rem;"></i>';
    } else if (['zip', 'rar', '7z', 'tar', 'gz', 'bz2'].includes(ext)) {
        return '<i class="fa-solid fa-file-zipper" style="color: #f77f00; font-size: 1.2rem;"></i>';
    } else if (['pdf', 'doc', 'docx', 'txt', 'csv', 'xlsx', 'pptx', 'json', 'py', 'js', 'html', 'css'].includes(ext)) {
        return '<i class="fa-solid fa-file-lines" style="color: #00ff88; font-size: 1.2rem;"></i>';
    }
    return '<i class="fa-solid fa-file" style="color: #8b9bb4; font-size: 1.2rem;"></i>';
}

// The media extension lists — the one source shared by isMedia() (the
// media filter + the play button) and previewMedia() (the modal's
// branch dispatch), so the two can never drift apart. mov/m4v and
// m4a/aac/opus ride the iPhone/web-media wave (false-negative fixes);
// avi stays listed — ArtPlayer's in-modal error fallback owns its
// failure path now (a message + the download way out) instead of a
// black screen. The image branch keeps the native <img>.
const VIDEO_EXTS = ['mp4', 'webm', 'mkv', 'avi', 'mov', 'm4v'];
const AUDIO_EXTS = ['mp3', 'wav', 'flac', 'ogg', 'm4a', 'aac', 'opus'];
const IMAGE_EXTS = ['jpg', 'jpeg', 'png', 'gif', 'webp', 'svg'];

// The preview modal's open ArtPlayer instance, if any — destroyed on
// the modal close (closeModal) and on the error fallback, so a closed
// modal never keeps decoding or holding its media element.
let currentArt = null;

function isMedia(name) {
    const ext = name.split('.').pop().toLowerCase();
    return VIDEO_EXTS.includes(ext) || AUDIO_EXTS.includes(ext) || IMAGE_EXTS.includes(ext);
}

function isDocument(name) {
    const ext = name.split('.').pop().toLowerCase();
    return ['pdf', 'doc', 'docx', 'txt', 'md', 'csv', 'xlsx', 'pptx', 'json', 'xml', 'py', 'js', 'html', 'css', 'sql', 'sh'].includes(ext);
}

function previewMedia(fileName) {
    const decoded = decodeURIComponent(fileName);
    const ext = decoded.split('.').pop().toLowerCase();
    const modal = document.getElementById("media-modal");
    const modalTitle = document.getElementById("modal-title");
    const modalBody = document.getElementById("modal-body");
    const modalDownload = document.getElementById("modal-download");

    modalTitle.innerText = decoded;
    const url = apiUrl(`/api/download/${fileName}`);
    // The modal's always-present download way out (top-right, next to
    // the close button) — the same anchor the error fallback leans on.
    if (modalDownload) {
        modalDownload.href = url;
        modalDownload.hidden = false;
    }

    if (VIDEO_EXTS.includes(ext)) {
        // ArtPlayer 5.x (UMD, window.Artplayer — vendored at
        // static/js/vendor/artplayer.js): the in-modal player with the
        // full settings tray. hotkey stays off — its global arrow/space
        // bindings would fight the page while the modal is open. The
        // theme matches the accent-cyan. A decode failure (avi, mkv
        // codecs the browser lacks) tears the player down and renders
        // the fallback layer with the download link.
        modalBody.innerHTML = '<div class="art-player-host"></div>';
        const container = modalBody.querySelector('.art-player-host');
        const art = new Artplayer({
            container: container,
            url: url,
            autoplay: true,
            theme: '#00f3ff',
            setting: true,
            playbackRate: true,
            aspectRatio: true,
            flip: true,
            screenshot: true,
            pip: true,
            hotkey: false,
            fullscreen: true,
            fullscreenWeb: true,
            miniProgressBar: true,
            mutex: true,
        });
        currentArt = art;
        art.on('error', () => {
            destroyCurrentArt();
            renderMediaFallback(modalBody, fileName);
        });
    } else if (AUDIO_EXTS.includes(ext)) {
        modalBody.innerHTML = `<audio controls autoplay style="width: 100%; margin-top: 1.5rem;"><source src="${url}"></audio>`;
        // With <source> children the load error fires on the source
        // element, not the audio element — the capture-phase listener
        // sees both.
        const audio = modalBody.querySelector('audio');
        if (audio) audio.addEventListener('error', () => renderMediaFallback(modalBody, fileName), true);
    } else {
        modalBody.innerHTML = `<img src="${url}" style="max-width: 100%; max-height: 520px; border-radius: 10px; display: block; margin: 0 auto; box-shadow: 0 0 30px rgba(0,243,255,0.25);">`;
    }

    modal.style.display = "flex";
}

// The preview's error fallback (a format the browser cannot decode):
// the message plus the download way out — never a silent black modal.
function renderMediaFallback(container, encodedName) {
    const url = apiUrl(`/api/download/${encodedName}`);
    container.innerHTML = `
        <div class="media-fallback">
            <i class="fa-solid fa-triangle-exclamation"></i>
            <p>${t('index.play_failed')}</p>
            <a class="btn btn-primary" href="${url}" download><i class="fa-solid fa-download"></i> ${t('index.download_instead')}</a>
        </div>
    `;
}

function destroyCurrentArt() {
    if (currentArt) {
        try { currentArt.destroy(); } catch (err) { /* already gone */ }
        currentArt = null;
    }
}

function closeModal() {
    destroyCurrentArt();
    const modal = document.getElementById("media-modal");
    if (modal) {
        const download = document.getElementById("modal-download");
        if (download) download.hidden = true;
        document.getElementById("modal-body").innerHTML = "";
        modal.style.display = "none";
    }
}

async function deleteFile(encodedName) {
    const fileName = decodeURIComponent(encodedName);
    // The confirm copy mirrors the server's real delete semantics
    // (/api/stats `remote_delete`): remote-delete backends remove the
    // cloud object, telegram keeps the remote copy (Python parity).
    // In multi-volume mode the volume name scopes the action (K23).
    const volumeNote = volumeState.multi && volumeState.current
        ? t('index.volume_note', { name: volumeState.current }) : "";
    const message = backendState.remoteDelete
        ? t('index.delete_confirm_remote', { name: fileName, volume: volumeNote })
        : t('index.delete_confirm_local', { name: fileName, volume: volumeNote });
    if (!confirm(message)) {
        return;
    }

    try {
        const res = await fetch(apiUrl("/api/delete"), {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ filename: fileName })
        });

        if (res.ok) {
            loadDriveData();
        } else {
            alert(t('index.delete_failed_alert'));
        }
    } catch (err) {
        console.error("Delete error:", err);
    }
}

function setupSearch() {
    const searchInput = document.getElementById("search-input");
    if (searchInput) {
        searchInput.addEventListener("input", () => {
            applyCurrentFilter(true);
        });
    }
}

function setupDropZone() {
    const dropZone = document.getElementById("drop-zone");
    if (!dropZone) return;
    
    ['dragenter', 'dragover'].forEach(name => {
        dropZone.addEventListener(name, (e) => {
            e.preventDefault();
            dropZone.classList.add('dragover');
        });
    });

    ['dragleave', 'drop'].forEach(name => {
        dropZone.addEventListener(name, (e) => {
            e.preventDefault();
            dropZone.classList.remove('dragover');
        });
    });

    dropZone.addEventListener('drop', (e) => {
        const files = e.dataTransfer.files;
        if (files.length > 0) {
            uploadFiles(files);
        }
    });

    dropZone.addEventListener('click', () => {
        document.getElementById('file-upload').click();
    });
}

function handleFileUpload(input) {
    if (input.files.length > 0) {
        uploadFiles(input.files);
    }
}

async function uploadFiles(files) {
    for (const file of files) {
        const formData = new FormData();
        formData.append("file", file);

        try {
            const res = await fetch(apiUrl("/api/upload"), {
                method: "POST",
                body: formData
            });
            if (res.ok) {
                console.log(`Uploaded ${file.name}`);
            }
        } catch (err) {
            console.error("Upload error:", err);
        }
    }
    loadDriveData();
}

// The file row's copy-link action: the absolute URL of the download
// endpoint (volume-scoped via apiUrl — the clipboard carries exactly
// the URL the download button would fetch). The Clipboard API first
// (the dashboard runs on a secure context), the temporary-textarea
// fallback second, an error toast last — the copy never dies silently.
async function copyLink(encodedName) {
    const url = location.origin + apiUrl(`/api/download/${encodedName}`);
    if (typeof navigator !== 'undefined' && navigator.clipboard && navigator.clipboard.writeText) {
        try {
            await navigator.clipboard.writeText(url);
            showToast('ok', t('index.link_copied'));
            return;
        } catch (err) { /* denied / busy — the legacy path takes over */ }
    }
    try {
        legacyCopyText(url);
        showToast('ok', t('index.link_copied'));
    } catch (err) {
        console.error("Copy failed:", err);
        showToast('err', t('index.copy_failed'));
    }
}

// The execCommand fallback (denied clipboard permission, missing API):
// a temporary off-screen textarea, select, copy, remove.
function legacyCopyText(text) {
    const area = document.createElement('textarea');
    area.value = text;
    area.style.position = 'fixed';
    area.style.opacity = '0';
    document.body.appendChild(area);
    area.select();
    let ok = false;
    try {
        ok = document.execCommand('copy');
    } finally {
        area.remove();
    }
    if (!ok) throw new Error('execCommand copy returned false');
}

// The toast (the volumes.js twin — same markup and classes, the
// style.css #toast-host rules are global): created on demand, one slot
// per message, 4s self-destruct.
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

function formatBytes(bytes, decimals = 2) {
    if (bytes === 0) return '0 Bytes';
    const k = 1024;
    const dm = decimals < 0 ? 0 : decimals;
    const sizes = ['Bytes', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(dm)) + ' ' + sizes[i];
}

function escapeHtml(text) {
    return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
