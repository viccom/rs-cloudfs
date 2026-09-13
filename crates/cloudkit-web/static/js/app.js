let allFiles = [];
let currentFilter = 'all';

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

// Display names per backend spelling (the /api/stats `backend` values).
const BACKEND_LABELS = {
    telegram: 'Telegram MTProto',
    baidu: 'Baidu Netdisk',
    local: 'Local Disk',
};

function backendDisplayName(backend) {
    return BACKEND_LABELS[backend] || 'your cloud drive';
}

document.addEventListener("DOMContentLoaded", async () => {
    await detectMultiVolumeMode();
    loadDriveData();
    setupDropZone();
    setupSearch();
    // Auto-refresh drive files and stats every 4 seconds
    setInterval(loadDriveData, 4000);
});

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
                updateSummaryCard();
            }
        }
    } catch (err) {
        console.error("Mode detection failed:", err);
    }
}

// One /api/volumes call carries everything the tabs and the summary
// card need (server reads db metadata — no per-volume N+1 polling).
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
                updateSummaryCard();
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
    bar.innerHTML = `${chips}<a href="/volumes" class="volume-tab volume-add" title="Manage volumes">＋</a>`;
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

// The cross-volume aggregate card: client-side sums over the ONE
// /api/volumes response (files / bytes / quota where the volumes
// report them).
function updateSummaryCard() {
    const card = document.getElementById("summary-card");
    if (!card) return;
    if (!volumeState.multi || !volumeState.volumes.length) {
        card.hidden = true;
        return;
    }
    card.hidden = false;
    let files = 0, bytes = 0, quotaUsed = 0, quotaTotal = 0, hasTotal = false;
    for (const v of volumeState.volumes) {
        if (typeof v.total_files === "number") files += v.total_files;
        if (typeof v.total_bytes === "number") bytes += v.total_bytes;
        if (typeof v.quota_used === "number") quotaUsed += v.quota_used;
        if (typeof v.quota_total === "number") {
            quotaTotal += v.quota_total;
            hasTotal = true;
        }
    }
    const sizeEl = document.getElementById("summary-size");
    if (sizeEl) sizeEl.innerText = formatBytes(bytes);
    const detailEl = document.getElementById("summary-detail");
    if (detailEl) {
        let detail = `All volumes: ${files} file${files === 1 ? "" : "s"}`;
        if (hasTotal) detail += ` • ${formatBytes(quotaUsed)} / ${formatBytes(quotaTotal)}`;
        detailEl.innerText = detail;
    }
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

function updateStorageCard(stats, indexedStr) {
    const detailEl = document.getElementById("storage-detail");
    const barEl = document.getElementById("storage-bar");
    const percentEl = document.getElementById("storage-percent");
    if (!detailEl || !barEl || !percentEl) return;

    barEl.classList.remove("muted");

    // Local disk: no quota concept — the number is what the drive
    // indexes, the bar rests as a dim idle strip.
    if (backendState.backend === 'local') {
        detailEl.innerText = `${indexedStr} on local disk`;
        percentEl.innerText = '—';
        barEl.style.width = '100%';
        barEl.classList.add('muted');
        return;
    }

    // A real backend ceiling: the ratio is the true used/total share
    // (boot snapshot from /api/stats — informational, not live).
    if (Number.isFinite(stats.quota_total) && stats.quota_total > 0) {
        const used = stats.quota_used || 0;
        const pct = Math.min(100, Math.round((used / stats.quota_total) * 100));
        detailEl.innerText = `${formatBytes(used)} / ${formatBytes(stats.quota_total)}`;
        percentEl.innerText = `${pct}%`;
        barEl.style.width = `${pct}%`;
        return;
    }

    // Unlimited (telegram) or the quota snapshot is unavailable: there
    // is no honest ratio to show, so the bar rests empty.
    detailEl.innerText = `${indexedStr} / Unlimited`;
    percentEl.innerText = '∞';
    barEl.style.width = '0%';
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
        dropSub.innerText = `Instantly syncs to your Windows Drive & ${label}`;
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

    applyCurrentFilter();
}

function applyCurrentFilter() {
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

    renderFilesTable(filtered);
}

function renderFilesTable(files) {
    const tbody = document.getElementById("files-tbody");
    const countLabel = document.getElementById("file-count-label");
    if (countLabel) countLabel.innerText = `${files.length} items`;

    if (!files || files.length === 0) {
        const target = backendState.backend
            ? backendDisplayName(backendState.backend)
            : 'your cloud drive';
        tbody.innerHTML = `
            <tr>
                <td colspan="5" style="text-align: center; color: var(--text-muted); padding: 3rem;">
                    <i class="fa-solid fa-folder-open" style="font-size: 2.2rem; margin-bottom: 0.8rem; display: block; color: var(--accent-cyan); opacity: 0.6;"></i>
                    No files found in this view. Drag and drop files above to sync to ${escapeHtml(target)}!
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
            ? '<span class="badge-status badge-synced"><i class="fa-solid fa-circle-check"></i> Synced</span>'
            : '<span class="badge-status badge-uploading"><i class="fa-solid fa-rotate fa-spin"></i> Syncing</span>';

        const isDir = Boolean(file.is_dir);
        const encName = encodeURIComponent(file.name);
        // The delete semantics are server-declared (K4): a remote-delete
        // backend really removes the cloud object, telegram only drops
        // the local row.
        const deleteTitle = backendState.remoteDelete
            ? 'Delete from Cloud'
            : 'Remove from local index';

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
                    ${!isDir ? `<a href="${apiUrl(`/api/download/${encName}`)}" class="action-btn" title="Download"><i class="fa-solid fa-download"></i></a>` : ''}
                    ${!isDir && isMedia(file.name) ? `<button class="action-btn" title="Stream Online" onclick="previewMedia('${encName}')"><i class="fa-solid fa-play"></i></button>` : ''}
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

function isMedia(name) {
    const ext = name.split('.').pop().toLowerCase();
    return ['mp4', 'webm', 'mkv', 'avi', 'mp3', 'wav', 'flac', 'ogg', 'jpg', 'jpeg', 'png', 'gif', 'webp', 'svg'].includes(ext);
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

    modalTitle.innerText = decoded;
    const url = apiUrl(`/api/download/${fileName}`);

    if (['mp4', 'webm', 'mkv', 'avi'].includes(ext)) {
        modalBody.innerHTML = `<video controls autoplay style="width: 100%; border-radius: 10px; box-shadow: 0 0 25px rgba(0,243,255,0.2);"><source src="${url}"></video>`;
    } else if (['mp3', 'wav', 'flac', 'ogg'].includes(ext)) {
        modalBody.innerHTML = `<audio controls autoplay style="width: 100%; margin-top: 1.5rem;"><source src="${url}"></audio>`;
    } else {
        modalBody.innerHTML = `<img src="${url}" style="max-width: 100%; max-height: 520px; border-radius: 10px; display: block; margin: 0 auto; box-shadow: 0 0 30px rgba(0,243,255,0.25);">`;
    }

    modal.style.display = "flex";
}

function closeModal() {
    const modal = document.getElementById("media-modal");
    if (modal) {
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
        ? ` in volume "${volumeState.current}"` : "";
    const message = backendState.remoteDelete
        ? `Are you sure you want to delete "${fileName}"${volumeNote}? This will also delete the file from the cloud backend.`
        : `Are you sure you want to remove "${fileName}"${volumeNote} from the local index? The cloud copy is kept.`;
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
            alert("Could not delete file from cloud.");
        }
    } catch (err) {
        console.error("Delete error:", err);
    }
}

function setupSearch() {
    const searchInput = document.getElementById("search-input");
    if (searchInput) {
        searchInput.addEventListener("input", () => {
            applyCurrentFilter();
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
