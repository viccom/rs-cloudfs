// Bilingual UI (zh/en) for the dashboard and the volume management page.
// The served HTML keeps its English text as the initial content — the
// Rust contract tests read the raw markup — and this script overlays the
// active language on DOMContentLoaded via the data-i18n family of
// attributes (textContent / placeholder / title). Strings rendered by
// app.js / volumes.js go through t() at render time, so the 4-second
// polls re-render in the active language too. Switching languages is a
// full reload (setLang stores the choice and reloads) — a deliberate
// trade: one code path, no partial-repaint state bugs.
//
// Language resolution: localStorage `cydrive.lang` first, then the
// browser's navigator.language (zh* → zh), defaulting to en.

const LANG = {
    en: {
        // --- sidebar / navigation -------------------------------------
        'sidebar.views': 'Views',
        // The config-side pages' mid-sidebar section label (the views
        // label's twin — the pages below it are flat nav-items too).
        'sidebar.section_config': 'Configuration',
        'page.files': 'Files',
        // The page pill's second segment: the config SECTION (the
        // sidebar's config section names its pages exactly).
        'pages.config': 'Configuration',
        'config.volumes': 'Volume Management',
        'config.system': 'System Parameters',
        'config.readonly': 'Read-only',

        // --- shared ---------------------------------------------------
        'common.cancel': 'Cancel',
        'common.continue': 'Continue',
        'common.refresh': 'Refresh',
        'common.done': 'Done.',
        'common.deleted': 'Deleted.',
        'common.unknown': 'unknown',
        'common.request_failed': 'request failed: {error}',
        'common.all_volumes': 'All volumes',
        'backend.telegram': 'Telegram MTProto',
        'backend.baidu': 'Baidu Netdisk',
        'backend.local': 'Local Disk',
        'backend.fallback': 'your cloud drive',
        'table.name': 'Name',
        'table.size': 'Size',
        'table.status': 'Status',
        'table.last_modified': 'Last Modified',
        'table.actions': 'Actions',
        'table.backend': 'Backend',
        'table.drive': 'Drive',
        'table.pending': 'Pending',

        // --- index (usage-state dashboard) -----------------------------
        'index.topbar_subtitle': "Files on this instance's storage volumes",
        'index.search_placeholder': 'Search files, documents, videos in CyDrive...',
        'index.upload_file': 'Upload File',
        'index.refresh_drive': 'Refresh Drive',
        'index.total_files': 'Total Files Stored',
        'index.total_size': 'Total Cloud Volume',
        'index.active': 'Active',
        'webdav.running': 'Running',
        'index.webdav_service': 'WebDAV Service',
        'index.drop_here': 'Drag & Drop Files Here',
        'index.drop_sub': 'Instantly syncs to your Windows Drive & Telegram Cloud',
        'index.drop_sub_backend': 'Instantly syncs to your Windows Drive & {backend}',
        'index.cloud_files': 'Cloud Files & Directories',
        'index.items': 'items',
        'index.loading_files': 'Loading CyDrive files...',
        'index.file_preview': 'File Preview',
        'index.mounted_on': 'Mounted on',
        'index.on_local_disk': '{size} on local disk',
        'index.size_unlimited': '{size} / Unlimited',
        'index.empty_files': 'No files found in this view. Drag and drop files above to sync to {target}!',
        'index.manage_volumes': 'Manage volumes',
        'index.media_gallery': 'Media Gallery',
        'index.documents': 'Documents',
        'index.download': 'Download',
        'index.stream': 'Stream Online',
        'index.delete_remote': 'Delete from Cloud',
        'index.delete_local': 'Remove from local index',
        'index.volume_note': ' in volume "{name}"',
        'index.delete_confirm_remote': 'Are you sure you want to delete "{name}"{volume}? This will also delete the file from the cloud backend.',
        'index.delete_confirm_local': 'Are you sure you want to remove "{name}"{volume} from the local index? The cloud copy is kept.',
        'index.delete_failed_alert': 'Could not delete file from cloud.',
        'status.synced': 'Synced',
        'status.syncing': 'Syncing',

        // --- the files table's client-side pagination --------------------
        'pagination.prev': 'Prev',
        'pagination.next': 'Next',
        'pagination.page_x_of_y': 'Page {x} / {y}',
        'pagination.total': '{n} total',
        'pagination.per_page': '{n} / page',

        // --- volumes (configuration-state page) ------------------------
        'volumes.title': 'Volume Management',
        'volumes.subtitle': 'Registered storage volumes served by this instance',
        'volumes.add_volume': 'Add Volume',
        'volumes.add_volume_title': 'Create a new volume file and assemble it',
        'volumes.edit_volume': 'Edit Volume',
        'volumes.create_volume': 'Create volume',
        'volumes.save_changes': 'Save changes',
        'volumes.creating': 'Creating…',
        'volumes.saving': 'Saving…',
        'volumes.storage_volumes': 'Storage Volumes',
        'volumes.loading': 'Loading volumes...',
        'volumes.volume_one': 'volume',
        'volumes.volume_many': 'volumes',
        'volumes.stat.volumes': 'Volumes',
        'volumes.stat.running': 'Running',
        'volumes.stat.failed': 'Failed',
        'volumes.stat.pending': 'Pending Uploads',
        'volumes.status.invalid': 'Invalid',
        'volumes.status.failed': 'Failed',
        'volumes.status.running': 'Running',
        'volumes.status.stopped': 'Stopped',
        'volumes.status.disabled': 'Disabled',
        'volumes.loading_error': 'The volume listing failed (HTTP {status}) — this page needs a multi-volume instance.',
        'volumes.empty': 'No volume files are configured on this instance.',
        'volumes.actions.refresh': 'Refresh',
        'volumes.actions.edit': 'Edit',
        'volumes.actions.unmount': 'Unmount',
        'volumes.actions.disable': 'Disable',
        'volumes.actions.enable': 'Enable',
        'volumes.actions.delete': 'Delete',
        'volumes.tip.edit_running': "Edit this volume's configuration (prefilled from its file; saving re-assembles it)",
        'volumes.tip.edit': "Edit this volume's configuration (prefilled from its file)",
        'volumes.tip.unmount': 'Unmount and unregister now (the volume file stays on disk)',
        'volumes.tip.disable': 'Write enabled = false to the volume file and unmount it (survives restarts)',
        'volumes.tip.enable': 'Write enabled = true and assemble the volume now',
        'volumes.tip.delete': "Delete this volume for good (two-step confirmation; its local data directory stays unless you tick purge)",
        'volumes.tip.delete_invalid': 'the volume file cannot be parsed — fix it by hand (or delete volumes/{name}.toml manually)',
        'volumes.tip.refresh': "Rebuild this volume's index from its remote backend (idempotent, runs in the background)",
        'volumes.note.refresh_telegram': "Refresh is not supported for the telegram backend — its db IS the index (the shadow index); use `cydrive sync` to replicate it to another instance instead",
        'volumes.note.refresh_encrypted': "Refresh refuses encrypted instances — the backend only sees ciphertext containers; use `cydrive sync` instead (the sync payload carries the encrypted row semantics)",
        'volumes.confirm.unmount': 'Unmount volume "{name}"?\nIts drive disappears immediately; the volume file stays on disk (re-enable anytime).',
        'volumes.confirm.disable': 'Disable volume "{name}"?\nThis writes enabled = false to its file (it stays disabled across restarts) and unmounts it.',

        // --- the volume form card (P3/P4) -------------------------------
        'volumes.form.name': 'Volume name',
        'volumes.form.drive_letter': 'Drive letter',
        'volumes.form.enabled_at_create': 'Enabled at create',
        'volumes.form.hint_name': 'a–z, 0–9, _ and - ; starts with a letter, up to 32 chars',
        'volumes.form.hint_name_bad': 'names start with a lowercase letter; only a-z, 0-9, _ and -; up to 32 chars',
        'volumes.form.hint_drive': 'empty = the volume serves /vol/<name> only',
        'volumes.form.hint_enabled': 'off = write the file disabled',
        'volumes.form.backend': 'Backend',
        'volumes.form.cred_telegram': 'Telegram credentials',
        'volumes.form.cred_baidu': 'Baidu credentials',
        'volumes.form.cred_local': 'Local backend',
        'volumes.form.bot_token': 'Bot token',
        'volumes.form.chat_id': 'Chat ID',
        'volumes.form.app_key': 'App key',
        'volumes.form.app_secret': 'App secret',
        'volumes.form.access_token': 'Access token',
        'volumes.form.refresh_token': 'Refresh token',
        'volumes.form.app_root': 'App root',
        'volumes.form.local_root': 'Local root',
        'volumes.form.advanced': 'Advanced (empty fields keep the defaults)',
        'volumes.form.enable_encryption': 'Enable encryption',
        'volumes.form.encryption_password': 'Encryption password',
        'volumes.form.scheme': 'Scheme',
        'volumes.form.chunk_size': 'Chunk size (MB)',
        'volumes.form.sync_url': 'Sync URL',
        'volumes.form.sync_secret': 'Sync secret',
        'volumes.form.sync_interval': 'Sync interval (s)',
        'volumes.form.placeholder_name': 'lowercase slug, e.g. media',
        'volumes.form.placeholder_drive': 'optional, e.g. Q (empty = no drive claim)',
        'volumes.form.cred_set': 'Set (leave empty to keep)',
        'volumes.form.cred_unset': 'Not set',
        'volumes.form.notice_edit': 'Saving re-assembles the volume (REMOVE + ADD): the drive \
briefly disappears and pending uploads must drain first. The volume file is rewritten — \
hand-written comments are lost. Credential fields left empty keep their stored values.',
        'volumes.form.name_required': 'Pick a volume name first: a lowercase letter, then a-z/0-9/_/- , up to 32 characters.',
        'volumes.form.required': 'The {field} field is required for this backend.',
        'volumes.form.not_whole': '"{value}" is not a whole number — the {key} field takes digits only.',
        'volumes.form.confirm_pending': 'Volume "{name}" has {count} pending upload(s).\nSaving re-assembles the volume (REMOVE + ADD): the update waits for the queue to drain (or is refused while uploads keep arriving).\n\nProceed?',

        // --- the delete modal (P5) ---------------------------------------
        'volumes.delm.title': 'Delete Volume',
        'volumes.delm.file_1': 'the volume file',
        'volumes.delm.file_2': '— and, while the volume runs, its registration and drive',
        'volumes.delm.keep_local_1': "the volume's local data directory (its db, cache and local root) — ",
        'volumes.delm.kept_by_default': 'kept by default',
        'volumes.delm.keep_remote_1': "all remote data on the volume's backend — ",
        'volumes.delm.never_touched': 'never touched',
        'volumes.delm.purge_1': "Also delete the volume's local data directory (",
        'volumes.delm.purge_2': ') — db, cache and local root go with it',
        'volumes.delm.warning': "This permanently removes the volume's configuration. Nothing on the remote backend is ever deleted; re-creating the volume re-attaches its data.",
        'volumes.delm.type_1': 'Type the volume name',
        'volumes.delm.type_2': 'to confirm the deletion:',
        'volumes.delm.confirm': 'Delete volume',
        'volumes.delm.removing': 'Removing…',

        // --- the single-volume explanation page ---------------------------
        'vsingle.p1_1': 'Single-volume mode: this instance serves ',
        'vsingle.p1_2': 'one volume defined by ',
        'vsingle.p1_3': ' — there is no volume registry to manage here.',
        'vsingle.p2_1': 'To manage volumes from the dashboard, run the instance in multi-volume mode: point ',
        'vsingle.p2_2': ' at a directory of ',
        'vsingle.p2_3': ' volume files and restart. The usage-state dashboard for the current volume is ',
        'vsingle.p2_4': 'on the home page',

        // --- the system parameters page (read-only, UI polish round 2) ----
        'system.title': 'System Parameters',
        'system.subtitle': "Read-only · from the instance's current configuration",
        'sys.instance': 'Instance',
        'sys.dashboard_url': 'Dashboard address',
        'sys.webdav_endpoints': 'WebDAV endpoints',
        'sys.volume_files': 'Volume files',
        'sys.running_volumes': 'Running volumes',
        'sys.volume_params': 'Storage Volume Parameters',
        'sys.loading': 'Loading configuration...',
        'sys.config_unavailable': 'the volume\'s configuration is unavailable (the SHOW command did not answer)',
        'sys.cred_set': 'Set',
        'sys.cred_unset': 'Not set',
        'sys.value.yes': 'yes',
        'sys.value.no': 'no',

        // Configuration-key display labels: the key → readable-label map
        // (a key missing here renders under its raw toml name).
        'syskey.enabled': 'Enabled',
        'syskey.drive_letter': 'Drive letter',
        'syskey.chunk_size_mb': 'Chunk size (MB)',
        'syskey.enable_encryption': 'Enable encryption',
        'syskey.encryption_scheme': 'Encryption scheme',
        'syskey.baidu_root': 'Baidu app root',
        'syskey.baidu_app_key': 'App key',
        'syskey.local_root': 'Local root',
        'syskey.storage_path': 'Storage path',
        'syskey.cache_path': 'Cache path',
        'syskey.db_path': 'Database path',
        'syskey.sync_url': 'Sync URL',
        'syskey.sync_interval_secs': 'Sync interval (s)',
        'syskey.chat_id': 'Chat ID',
        'syskey.bot_token': 'Bot token',
        'syskey.encryption_password': 'Encryption password',
        'syskey.sync_secret': 'Sync secret',
        'syskey.baidu_app_secret': 'App secret',
        'syskey.baidu_access_token': 'Access token',
        'syskey.baidu_refresh_token': 'Refresh token',

        // --- the system page's single-volume explanation ------------------
        'syssingle.p1_1': 'Single-volume mode: this instance serves ',
        'syssingle.p1_2': 'one volume defined by ',
        'syssingle.p1_3': " — its parameters are that file's content, and this read-only page serves multi-volume instances.",
    },

    zh: {
        // --- 侧栏 / 导航 ---------------------------------------------
        'sidebar.views': '视图',
        // 配置侧页面侧栏中部的节标签（视图标签的孪生——其下各项同为平铺
        // nav-item）。
        'sidebar.section_config': '配置',
        'page.files': '文件管理',
        // 页面 pill 第二段：配置「分区」（侧栏配置节精确点名各页）。
        'pages.config': '配置管理',
        'config.volumes': '卷管理',
        'config.system': '系统参数',
        'config.readonly': '只读',

        // --- 通用 ------------------------------------------------------
        'common.cancel': '取消',
        'common.continue': '继续',
        'common.refresh': '刷新',
        'common.done': '完成。',
        'common.deleted': '已删除。',
        'common.unknown': '未知',
        'common.request_failed': '请求失败：{error}',
        'common.all_volumes': '全部卷',
        'backend.telegram': 'Telegram MTProto',
        'backend.baidu': '百度网盘',
        'backend.local': '本地磁盘',
        'backend.fallback': '你的云盘',
        'table.name': '名称',
        'table.size': '大小',
        'table.status': '状态',
        'table.last_modified': '修改时间',
        'table.actions': '操作',
        'table.backend': '后端',
        'table.drive': '盘符',
        'table.pending': '待上传',

        // --- index（使用态仪表盘）---------------------------------------
        'index.topbar_subtitle': '本实例各存储卷上的文件',
        'index.search_placeholder': '在 CyDrive 中搜索文件、文档、视频…',
        'index.upload_file': '上传文件',
        'index.refresh_drive': '刷新云盘',
        'index.total_files': '文件总数',
        'index.total_size': '云端总容量',
        'index.active': '运行中',
        'webdav.running': '运行中',
        'index.webdav_service': 'WebDAV 服务',
        'index.drop_here': '拖拽文件到此处',
        'index.drop_sub': '即时同步至 Windows 磁盘与 Telegram 云端',
        'index.drop_sub_backend': '即时同步至 Windows 磁盘与 {backend}',
        'index.cloud_files': '云端文件与目录',
        'index.items': '项',
        'index.loading_files': '正在加载 CyDrive 文件…',
        'index.file_preview': '文件预览',
        'index.mounted_on': '挂载于',
        'index.on_local_disk': '本地磁盘已索引 {size}',
        'index.size_unlimited': '{size} / 无上限',
        'index.empty_files': '此视图中没有文件。拖拽文件到上方即可同步到 {target}！',
        'index.manage_volumes': '管理卷',
        'index.media_gallery': '媒体库',
        'index.documents': '文档',
        'index.download': '下载',
        'index.stream': '在线播放',
        'index.delete_remote': '从云端删除',
        'index.delete_local': '从本地索引移除',
        'index.volume_note': '（卷「{name}」中）',
        'index.delete_confirm_remote': '确定要删除「{name}」{volume}吗？这会同时从云端后端删除该文件。',
        'index.delete_confirm_local': '确定要从本地索引移除「{name}」{volume}吗？云端副本保留。',
        'index.delete_failed_alert': '无法从云端删除文件。',
        'status.synced': '已同步',
        'status.syncing': '同步中',

        // --- 文件表的客户端分页 -------------------------------------------
        'pagination.prev': '上一页',
        'pagination.next': '下一页',
        'pagination.page_x_of_y': '第 {x} / {y} 页',
        'pagination.total': '共 {n} 条',
        'pagination.per_page': '{n} / 页',

        // --- volumes（配置态管理页）--------------------------------------
        'volumes.title': '卷管理',
        'volumes.subtitle': '本实例承接的已注册存储卷',
        'volumes.add_volume': '添加卷',
        'volumes.add_volume_title': '创建新卷文件并装配',
        'volumes.edit_volume': '编辑卷',
        'volumes.create_volume': '创建卷',
        'volumes.save_changes': '保存修改',
        'volumes.creating': '创建中…',
        'volumes.saving': '保存中…',
        'volumes.storage_volumes': '存储卷',
        'volumes.loading': '正在加载卷…',
        'volumes.volume_one': '个卷',
        'volumes.volume_many': '个卷',
        'volumes.stat.volumes': '卷数',
        'volumes.stat.running': '运行中',
        'volumes.stat.failed': '失败',
        'volumes.stat.pending': '待上传',
        'volumes.status.invalid': '无效',
        'volumes.status.failed': '失败',
        'volumes.status.running': '运行中',
        'volumes.status.stopped': '已停止',
        'volumes.status.disabled': '已停用',
        'volumes.loading_error': '卷列表加载失败（HTTP {status}）——本页需要多卷模式实例。',
        'volumes.empty': '本实例未配置任何卷文件。',
        'volumes.actions.refresh': '刷新',
        'volumes.actions.edit': '编辑',
        'volumes.actions.unmount': '卸载',
        'volumes.actions.disable': '停用',
        'volumes.actions.enable': '启用',
        'volumes.actions.delete': '删除',
        'volumes.tip.edit_running': '编辑此卷的配置（从其文件预填；保存会重新装配）',
        'volumes.tip.edit': '编辑此卷的配置（从其文件预填）',
        'volumes.tip.unmount': '立即卸载并注销（卷文件保留在磁盘上）',
        'volumes.tip.disable': '向卷文件写入 enabled = false 并卸载（重启后保持停用）',
        'volumes.tip.enable': '写入 enabled = true 并立即装配该卷',
        'volumes.tip.delete': '彻底删除此卷（两步确认；不勾选清除时本地数据目录保留）',
        'volumes.tip.delete_invalid': '卷文件无法解析——请手工修复（或手动删除 volumes/{name}.toml）',
        'volumes.tip.refresh': '从远端后端重建此卷的索引（幂等，后台运行）',
        'volumes.note.refresh_telegram': 'telegram 后端不支持刷新——其数据库即索引（影子索引）；请改用 `cydrive sync` 复制到其他实例',
        'volumes.note.refresh_encrypted': '加密实例拒绝刷新——后端只见密文容器；请改用 `cydrive sync`（同步载荷携带加密行语义）',
        'volumes.confirm.unmount': '卸载卷「{name}」？\n其盘符立即消失；卷文件保留在磁盘上（可随时重新启用）。',
        'volumes.confirm.disable': '停用卷「{name}」？\n这会向其文件写入 enabled = false（重启后保持停用）并卸载该卷。',

        // --- 卷表单卡（P3/P4）---------------------------------------------
        'volumes.form.name': '卷名称',
        'volumes.form.drive_letter': '盘符',
        'volumes.form.enabled_at_create': '创建时启用',
        'volumes.form.hint_name': 'a–z、0–9、_ 和 -；以字母开头，最长 32 字符',
        'volumes.form.hint_name_bad': '名称以小写字母开头；仅限 a-z、0-9、_ 和 -；最长 32 字符',
        'volumes.form.hint_drive': '留空 = 卷仅通过 /vol/<name> 提供服务',
        'volumes.form.hint_enabled': '关闭 = 文件以停用状态写入',
        'volumes.form.backend': '后端',
        'volumes.form.cred_telegram': 'Telegram 凭据',
        'volumes.form.cred_baidu': '百度凭据',
        'volumes.form.cred_local': '本地后端',
        'volumes.form.bot_token': 'Bot token',
        'volumes.form.chat_id': 'Chat ID',
        'volumes.form.app_key': 'App key',
        'volumes.form.app_secret': 'App secret',
        'volumes.form.access_token': 'Access token',
        'volumes.form.refresh_token': 'Refresh token',
        'volumes.form.app_root': 'App root',
        'volumes.form.local_root': '本地根目录',
        'volumes.form.advanced': '高级（留空字段保留默认值）',
        'volumes.form.enable_encryption': '启用加密',
        'volumes.form.encryption_password': '加密密码',
        'volumes.form.scheme': '方案',
        'volumes.form.chunk_size': '分块大小（MB）',
        'volumes.form.sync_url': '同步 URL',
        'volumes.form.sync_secret': '同步密钥',
        'volumes.form.sync_interval': '同步间隔（秒）',
        'volumes.form.placeholder_name': '小写 slug，如 media',
        'volumes.form.placeholder_drive': '可选，如 Q（留空 = 不占盘符）',
        'volumes.form.cred_set': '已设置（留空 = 不修改）',
        'volumes.form.cred_unset': '未设置',
        'volumes.form.notice_edit': '保存会重新装配卷（REMOVE + ADD）：盘符短暂消失且必须先排空待上传。卷文件会被重写——手工注释丢失。凭据字段留空则保留已存值。',
        'volumes.form.name_required': '请先填写卷名：以小写字母开头，后接 a-z/0-9/_/-，最长 32 字符。',
        'volumes.form.required': '此后端需要填写「{field}」。',
        'volumes.form.not_whole': '「{value}」不是整数——{key} 字段只接受数字。',
        'volumes.form.confirm_pending': '卷「{name}」有 {count} 个待上传任务。\n保存会重新装配卷（REMOVE + ADD）：更新会等待队列排空（或在上传持续到达时被拒绝）。\n\n继续吗？',

        // --- 删除弹窗（P5）-------------------------------------------------
        'volumes.delm.title': '删除卷',
        'volumes.delm.file_1': '卷文件',
        'volumes.delm.file_2': '——卷运行中会连同其注册与盘符一并移除',
        'volumes.delm.keep_local_1': '卷的本地数据目录（数据库、缓存与本地根目录）——',
        'volumes.delm.kept_by_default': '默认保留',
        'volumes.delm.keep_remote_1': '卷后端的全部远端数据——',
        'volumes.delm.never_touched': '永不触碰',
        'volumes.delm.purge_1': '同时删除卷的本地数据目录（',
        'volumes.delm.purge_2': '）——数据库、缓存与本地根目录一并删除',
        'volumes.delm.warning': '此操作永久移除卷的配置。远端后端上的任何数据都不会被删除；重新创建卷即可重新挂接其数据。',
        'volumes.delm.type_1': '输入卷名',
        'volumes.delm.type_2': '以确认删除：',
        'volumes.delm.confirm': '删除卷',
        'volumes.delm.removing': '删除中…',

        // --- 单卷说明页 ------------------------------------------------------
        'vsingle.p1_1': '单卷模式：本实例仅服务',
        'vsingle.p1_2': '由',
        'vsingle.p1_3': '定义的那一个卷——此处没有卷注册表可管理。',
        'vsingle.p2_1': '要在仪表盘中管理卷，请以多卷模式运行实例：将',
        'vsingle.p2_2': '指向一个存放',
        'vsingle.p2_3': '卷文件的目录并重启。当前卷的使用态仪表盘在',
        'vsingle.p2_4': '首页',

        // --- 系统参数页（只读，UI polish 第二轮）---------------------------
        'system.title': '系统参数',
        'system.subtitle': '只读 · 来自实例当前配置',
        'sys.instance': '实例',
        'sys.dashboard_url': '仪表盘地址',
        'sys.webdav_endpoints': 'WebDAV 端点',
        'sys.volume_files': '卷文件数',
        'sys.running_volumes': '运行中卷数',
        'sys.volume_params': '存储卷参数',
        'sys.loading': '正在加载配置…',
        'sys.config_unavailable': '该卷的配置不可用（SHOW 命令未应答）',
        'sys.cred_set': '已设置',
        'sys.cred_unset': '未设置',
        'sys.value.yes': '是',
        'sys.value.no': '否',

        // 配置键展示标签：键 → 可读标签映射（未收录的键按原始 toml 键名显示）。
        'syskey.enabled': '启用',
        'syskey.drive_letter': '盘符',
        'syskey.chunk_size_mb': '分块大小（MB）',
        'syskey.enable_encryption': '启用加密',
        'syskey.encryption_scheme': '加密方案',
        'syskey.baidu_root': '百度应用根目录',
        'syskey.baidu_app_key': 'App key',
        'syskey.local_root': '本地根目录',
        'syskey.storage_path': '存储路径',
        'syskey.cache_path': '缓存路径',
        'syskey.db_path': '数据库路径',
        'syskey.sync_url': '同步 URL',
        'syskey.sync_interval_secs': '同步间隔（秒）',
        'syskey.chat_id': 'Chat ID',
        'syskey.bot_token': 'Bot token',
        'syskey.encryption_password': '加密密码',
        'syskey.sync_secret': '同步密钥',
        'syskey.baidu_app_secret': 'App secret',
        'syskey.baidu_access_token': 'Access token',
        'syskey.baidu_refresh_token': 'Refresh token',

        // --- 系统参数页的单卷说明 -------------------------------------------
        'syssingle.p1_1': '单卷模式：本实例仅服务',
        'syssingle.p1_2': '由',
        'syssingle.p1_3': '定义的那一个卷——其参数即该文件的内容；本只读页面服务多卷模式实例。',
    },
};

// The active language: the stored choice wins, then the browser's
// locale (zh* → zh), then en.
function currentLang() {
    try {
        const stored = localStorage.getItem('cydrive.lang');
        if (stored === 'zh' || stored === 'en') return stored;
    } catch (err) { /* storage off */ }
    const nav = (typeof navigator !== 'undefined' && navigator.language) || 'en';
    return String(nav).toLowerCase().startsWith('zh') ? 'zh' : 'en';
}

// The dictionary lookup with {name} placeholder substitution. A missing
// key falls back to en, then to the key itself (never undefined in the
// DOM).
function t(key, params) {
    const lang = currentLang();
    let text = LANG[lang] && LANG[lang][key] !== undefined
        ? LANG[lang][key]
        : LANG.en[key];
    if (text === undefined) return key;
    if (params) {
        for (const [name, value] of Object.entries(params)) {
            text = text.split('{' + name + '}').join(String(value));
        }
    }
    return text;
}

// Overlays the active language onto the static (English) markup: text
// via data-i18n, placeholders via data-i18n-placeholder, tooltips via
// data-i18n-title. Also keeps <html lang> and the pill's active segment
// in step.
function applyI18n() {
    const lang = currentLang();
    document.documentElement.setAttribute('lang', lang === 'zh' ? 'zh-CN' : 'en');
    document.querySelectorAll('[data-i18n]').forEach(el => {
        el.textContent = t(el.dataset.i18n);
    });
    document.querySelectorAll('[data-i18n-placeholder]').forEach(el => {
        el.setAttribute('placeholder', t(el.dataset.i18nPlaceholder));
    });
    document.querySelectorAll('[data-i18n-title]').forEach(el => {
        el.setAttribute('title', t(el.dataset.i18nTitle));
    });
    applyLangPill();
}

// The [中|EN] pill: the active language's segment highlights.
function applyLangPill() {
    const lang = currentLang();
    document.querySelectorAll('.lang-pill [data-lang]').forEach(seg => {
        seg.classList.toggle('active', seg.dataset.lang === lang);
    });
}

// Stores the choice and reloads — a full redraw so no partially
// re-rendered state (tables, toasts, forms) lags the switch.
function setLang(lang) {
    if (lang !== 'zh' && lang !== 'en') return;
    try { localStorage.setItem('cydrive.lang', lang); } catch (err) { /* storage off */ }
    location.reload();
}

document.addEventListener('DOMContentLoaded', applyI18n);
