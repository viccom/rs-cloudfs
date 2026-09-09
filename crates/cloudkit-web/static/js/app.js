let allFiles = [];
let currentFilter = 'all';

// The active backend's identity, refreshed from every /api/stats poll
// (server-reported — the UI never guesses which backend is running).
let backendState = {
    backend: null,        // 'telegram' | 'baidu' | 'local' | ...
    remoteDelete: false,  // true: a delete removes the cloud object too
    configured: false,    // telegram-side credentials present
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

document.addEventListener("DOMContentLoaded", () => {
    loadDriveData();
    setupDropZone();
    setupSearch();
    // Auto-refresh drive files and stats every 4 seconds
    setInterval(loadDriveData, 4000);
});

async function loadDriveData() {
    try {
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
    } catch (err) {
        console.error("Error loading drive data:", err);
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
                    ${!isDir ? `<a href="/api/download/${encName}" class="action-btn" title="Download"><i class="fa-solid fa-download"></i></a>` : ''}
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
    const url = `/api/download/${fileName}`;

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
    const message = backendState.remoteDelete
        ? `Are you sure you want to delete "${fileName}"? This will also delete the file from the cloud backend.`
        : `Are you sure you want to remove "${fileName}" from the local index? The cloud copy is kept.`;
    if (!confirm(message)) {
        return;
    }

    try {
        const res = await fetch("/api/delete", {
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
            const res = await fetch("/api/upload", {
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
