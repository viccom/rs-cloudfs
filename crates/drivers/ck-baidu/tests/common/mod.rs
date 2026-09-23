//! MockBaidu——axum 内存百度后端（K16；Phase 2 Batch B1 测试基建，
//! B2 扩展上传三步曲/下载 dlink/CDN 面）。
//!
//! **落位注码**：Phase 2 执行计划 §3 Files 原文写 `tests/mock_backend.rs`——
//! Rust 集成测试的多套件共享基建必须落 `tests/common/mod.rs`（各套件
//! `mod common;` 引用），本模块即该计划的 mock baidu 后端落位。
//!
//! 行为语义（黄金参照：spike 实抓 + PCFS 源码，分歧以 spike 为准）：
//!
//! - **业务端点**（`/rest/2.0/xpan/file`、`/rest/2.0/xpan/nas`）校验
//!   query `access_token == current`，不匹配 → HTTP 200 + `{"errno":110}`
//!   （xpan 家族以 errno 而非 HTTP 状态承载业务错误）；
//! - **错误注入队列**优先于 token 校验与方法分发（下一个业务请求顶
//!   错误）——注意注入须在 driver 构造（uinfo 首调）之后进行，否则会
//!   顶掉 connect；
//! - **oauth 端点**（`/oauth/2.0/token`）校验 `refresh_token == current`：
//!   匹配则**一次一换**（轮换出全新 access/refresh 对，模拟 spike §1
//!   「refresh_token 一次一换、旧值即刻作废」实证），陈旧值 → HTTP 400 +
//!   `{"error":"invalid_grant"}`；
//! - **请求记录器**存 raw query/body 与关键头（content-type/user-agent/
//!   range），供三套件做表单**字节级断言**（`tests/metadata_ops.rs`）。
//!
//! B2 扩展（只加不改——既有路由行为与 B1 断言零变化）：
//!
//! - **上传三步曲**：`POST xpan/file?method=precreate`（uploadid 计数器
//!   签发 + block_list 记档；`mark_instant` 命中路径返回 return_type=2
//!   秒传腿）；`POST /rest/2.0/pcs/superfile2`（multipart 解析 partseq、
//!   会话分片累积、`bytes_received_total` 计数、`inject_upload_death`
//!   会话死亡注入）；`POST xpan/file?method=create`（isdir=0 腿校验
//!   **block_list 与 precreate 会话锁定声明一致**（不一致 → errno=31363，
//!   2026-09-08 真网探针实证——B2 返工裁决驱动）+ 分片齐全（缺片/分片
//!   md5 不符 errno=10，spike §3.3 实证形态）→ 组装文件入树，rtype=3
//!   覆盖语义）；superfile2 仍接受任意 partseq（真网实证接受未声明分片
//!   ——分片级校验不受 31363 约束影响）；
//! - **下载链路**：`GET xpan/file?method=download` → 302 Location
//!   `{base}/cdn/{fs_id}?expires=<mock时钟+TTL>`；`GET /cdn/{fs_id}` 校验
//!   netdisk UA + 有界 Range ≤4MiB（违反三约束之一 → 403 error_code=31326，
//!   spike §5 dl-try 矩阵），`CdnAuthMode::TokenRequired` 建模「直连 403、
//!   追加 access_token 后 206」两态；mock 时钟经 `advance_clock` 推进使旧
//!   链 expires 过期（token 救不了——重取 dlink 才可恢复）。
//!
//! B2 第五轮真网返工扩展（2026-09-08，**语义修正**——原 -8 建模废除）：
//!
//! - **目录 create 冲突形态**：`POST xpan/file?method=create` form
//!   `path=<已存在目录>&isdir=1` → **errno=0**（成功假象）+ 树中生成
//!   `<原名>_<时间戳>`（mock 时钟换算 `%Y%m%d_%H%M%S`）空副本目录——
//!   真网实证非 -8。由此「未预检」的驱动实现（直接 create 已存在层）在
//!   离线测试即可被 `entry_count_with_prefix` 检出（ghost 副本观测面）。
//!
//! Phase 8-B 0 字节 wire 真形补钉（2026-09-23，主会话活 token 真网探针
//! ——「桩照服务端真形建模」防线）：
//!
//! - **precreate 空数组 block_list → errno=2**（真网实测：0 字节发 `[]`
//!   恒拒且可重试耗尽；正确形态 = `[EMPTY_MD5]` 空串 MD5）；
//! - **create 对 `[EMPTY_MD5]` 唯一块声明免分片校验**（真网真形：0 字节
//!   无 superfile2 可传，precreate 声明即视为在位）→ 组装零字节入树。

#![allow(dead_code)] // 三个测试二进制各自编译本模块，未用到的访问器按二进制豁免

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use serde_json::{json, Value};

use ck_baidu::{BaiduParams, TokenStore, EMPTY_MD5};

// ---------------------------------------------------------------------------
// 路径与常量（断言与编排用）
// ---------------------------------------------------------------------------

pub const XPAN_FILE: &str = "/rest/2.0/xpan/file";
pub const XPAN_NAS: &str = "/rest/2.0/xpan/nas";
pub const OAUTH_TOKEN: &str = "/oauth/2.0/token";
/// superfile2 分片上传端点（PCS 域路径；spike api.rs:14）。
pub const PCS_SUPERFILE2: &str = "/rest/2.0/pcs/superfile2";
/// CDN 下载路径前缀（mock 302 Location 指向）。
pub const CDN_PREFIX: &str = "/cdn/";

/// 测试根（与生产缺省 `baidu_root` 同形态，K17）。
pub const MOCK_ROOT: &str = "/apps/cloudfs";
/// 假凭据（占位形态，绝非真实凭据——R3）。
pub const MOCK_APP_KEY: &str = "mock-app-key";
pub const MOCK_APP_SECRET: &str = "mock-app-secret";
pub const INITIAL_ACCESS_TOKEN: &str = "mock-access-0";
pub const INITIAL_REFRESH_TOKEN: &str = "mock-refresh-0";
/// uinfo 返回的测试 uid（VolumeId 断言用）。
pub const MOCK_UID: i64 = 1400000001;
/// spike common.rs:16 的 netdisk UA——client 构造照抄，wire 断言防漂移。
pub const NETDISK_UA: &str = "netdisk;P2SP;2.2.91.136;android-android";
/// 百度分片尺寸（spike §2/§6：上/下载统一 4MiB 有界）。
pub const CHUNK_4M: usize = 4 * 1024 * 1024;
/// mock 签发 dlink 的名义有效期（秒）——`advance_clock` 推进超过即旧链
/// 过期（CDN 403，与 access_token 无关）。
pub const MOCK_DLINK_TTL_SECS: i64 = 600;
/// 会话死亡注入的 error_code（mock 建模码，刻意避开全部已映射真实码族
/// ——驱动探活语义只判「error_code != 0」，不应绑定具体码值）。
pub const MOCK_DEAD_SESSION_ERROR_CODE: i64 = 91001;

// ---------------------------------------------------------------------------
// 状态模型
// ---------------------------------------------------------------------------

/// 内存文件树条目（形态对齐后端 list/meta 响应项字段名）。
#[derive(Debug, Clone)]
pub struct MockEntry {
    pub path: String,
    pub isdir: bool,
    pub size: i64,
    pub fs_id: i64,
    pub server_mtime: i64,
    pub server_filename: String,
    pub md5: String,
}

/// 请求记录（raw query/body + 关键头——字节级断言的观测面）。
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub http_method: String,
    pub path: String,
    pub query: String,
    /// body 的 lossy 字符串形态（表单/JSON 断言用；二进制载荷会失真，
    /// 保真走 `raw_body`）。
    pub body: String,
    /// body 原始字节（multipart 净荷解析——superfile2 断言用）。
    pub raw_body: Vec<u8>,
    pub content_type: Option<String>,
    pub user_agent: Option<String>,
    /// Range 请求头原样（CDN 有界分片断言用；非 Range 请求为 None）。
    pub range_header: Option<String>,
}

/// superfile2 分片请求的解析视图（差集/表单断言用）。
#[derive(Debug, Clone)]
pub struct Superfile2Record {
    pub uploadid: String,
    pub partseq: i64,
    /// multipart `file` part 的净荷字节数（不含 multipart 协议开销）。
    pub part_bytes: usize,
    /// multipart Content-Disposition 的 name（黄金参照 `file`）。
    pub field_name: String,
    /// multipart part 的 Content-Type（黄金参照 octet-stream）。
    pub part_mime: Option<String>,
    pub user_agent: Option<String>,
}

/// CDN 授权形态（spike §5：直连与「追加 access_token」两态都出现过）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CdnAuthMode {
    /// 直连可用（spike qps 首轮形态）。
    #[default]
    Direct,
    /// 直连 403 31326、query 追加 access_token 后 206（spike qps 次轮形态）。
    TokenRequired,
}

/// 上传会话（precreate 建立；superfile2 累积分片；create 消费终结）。
#[derive(Debug, Default)]
struct UploadSession {
    path: String,
    size: i64,
    /// precreate 记档的分片 md5 列表（create 逐片比对）。
    block_md5: Vec<String>,
    /// partseq → 分片净荷（create 按序拼接为文件内容）。
    parts: BTreeMap<i64, Vec<u8>>,
}

struct MockState {
    entries: Vec<MockEntry>,
    access_token: String,
    refresh_token: String,
    recorded: Vec<RecordedRequest>,
    inject: VecDeque<i64>,
    refresh_count: u64,
    uid: i64,
    quota_used: i64,
    quota_total: i64,
    next_fs_id: i64,
    // --- B2 上传三步曲 ---
    sessions: BTreeMap<String, UploadSession>,
    /// superfile2 收到的分片净荷字节总数（conformance ⑦ / 差集观测点）。
    bytes_received_total: u64,
    next_uploadid: u64,
    /// 会话死亡注入：uploadid → 待顶替下一分片（一次性消费）。
    dead_uploads: BTreeMap<String, i64>,
    /// 秒传路径集（precreate 命中 → return_type=2 + fs_id 直接收尾）。
    instant_paths: BTreeSet<String>,
    /// 索引传播延迟注入：下一次 method=meta 顶 -9（一次性；真网实证
    /// create 后 meta 单点查询短暂不可见而 list 即时——close 的 list
    /// 兜底路径的触发面）。
    fail_next_meta: bool,
    // --- B2 下载链路 ---
    /// mock 自身 base（302 Location 绝对 URL 拼接）。
    base_url: String,
    /// mock 时钟（unix 秒；CDN expires 判定基准，可注入推进）。
    mock_clock: i64,
    cdn_mode: CdnAuthMode,
    /// fs_id → 文件内容（create 组装 / seed 下载）。
    file_blobs: BTreeMap<i64, Vec<u8>>,
}

/// mock 后端句柄（访问器均同步短临界区，无 await 持锁）。
pub struct MockBaidu {
    state: Arc<Mutex<MockState>>,
    base_url: String,
}

impl MockBaidu {
    /// 启动内存后端（127.0.0.1 随机端口），返回 (句柄, base_url)。
    pub async fn start() -> (MockBaidu, String) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(1_757_000_000);
        let state = Arc::new(Mutex::new(MockState {
            entries: Vec::new(),
            access_token: INITIAL_ACCESS_TOKEN.to_string(),
            refresh_token: INITIAL_REFRESH_TOKEN.to_string(),
            recorded: Vec::new(),
            inject: VecDeque::new(),
            refresh_count: 0,
            uid: MOCK_UID,
            quota_used: 123456789,
            quota_total: 1099511627776,
            next_fs_id: 671337245231600, // spike 实测量级（50bit fs_id 家族）
            sessions: BTreeMap::new(),
            bytes_received_total: 0,
            next_uploadid: 0,
            dead_uploads: BTreeMap::new(),
            instant_paths: BTreeSet::new(),
            fail_next_meta: false,
            base_url: String::new(), // 占位，bind 后回填
            mock_clock: now,
            cdn_mode: CdnAuthMode::Direct,
            file_blobs: BTreeMap::new(),
        }));
        let app = axum::Router::new()
            .route(XPAN_FILE, get(xpan_file).post(xpan_file))
            .route(XPAN_NAS, get(xpan_nas))
            .route(OAUTH_TOKEN, get(oauth_token))
            .route(PCS_SUPERFILE2, axum::routing::post(superfile2))
            .route("/cdn/{fs_id}", get(cdn_get))
            // B2 基建修复（非语义变更）：axum 默认 2MB body 限制挡住
            // superfile2 的 4MiB 分片净荷（Bytes extractor 在 limit 处
            // 413——分片根本到不了处理器，既有 multipart 语义无从发生）。
            // 8MB = 4MiB 分片 + multipart 协议开销余量。
            .layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock baidu");
        let addr = listener.local_addr().expect("mock baidu local addr");
        let base_url = format!("http://{addr}");
        state.lock().unwrap().base_url = base_url.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock baidu accept loop");
        });
        (
            MockBaidu {
                state,
                base_url: base_url.clone(),
            },
            base_url,
        )
    }

    // -- 种子/编排访问器 ----------------------------------------------------

    /// 播种完整条目（全字段控制）。
    pub fn seed_entry(&self, entry: MockEntry) {
        self.state.lock().unwrap().entries.push(entry);
    }

    /// 播种目录，返回分配的 fs_id。
    pub fn seed_dir(&self, path: &str) -> i64 {
        let mut st = self.state.lock().unwrap();
        let fs_id = st.next_fs_id;
        st.next_fs_id += 1;
        st.entries.push(MockEntry {
            path: path.to_string(),
            isdir: true,
            size: 0,
            fs_id,
            server_mtime: 1757000000,
            server_filename: path.rsplit('/').next().unwrap_or_default().to_string(),
            md5: String::new(),
        });
        fs_id
    }

    /// 播种文件，返回分配的 fs_id（Entry.handle 断言用）。
    pub fn seed_file(&self, path: &str, size: i64, server_mtime: i64) -> i64 {
        let mut st = self.state.lock().unwrap();
        let fs_id = st.next_fs_id;
        st.next_fs_id += 1;
        st.entries.push(MockEntry {
            path: path.to_string(),
            isdir: false,
            size,
            fs_id,
            server_mtime,
            server_filename: path.rsplit('/').next().unwrap_or_default().to_string(),
            md5: format!("{fs_id:032x}"),
        });
        fs_id
    }

    /// 播种带内容的文件（下载链路测试用——独立于上传路径）：内容入
    /// `file_blobs`，返回 fs_id。
    pub fn seed_file_bytes(&self, path: &str, content: &[u8], server_mtime: i64) -> i64 {
        let fs_id = self.seed_file(path, content.len() as i64, server_mtime);
        self.state
            .lock()
            .unwrap()
            .file_blobs
            .insert(fs_id, content.to_vec());
        fs_id
    }

    /// 顶替当前有效 access_token（使旧值失效——110 场景编排）。
    pub fn set_access_token(&self, token: &str) {
        self.state.lock().unwrap().access_token = token.to_string();
    }

    /// 注入 errno：下一个**业务**请求（xpan/file、xpan/nas）顶此错误。
    pub fn inject_errno(&self, errno: i64) {
        self.state.lock().unwrap().inject.push_back(errno);
    }

    pub fn set_quota(&self, used: i64, total: i64) {
        let mut st = self.state.lock().unwrap();
        st.quota_used = used;
        st.quota_total = total;
    }

    // -- B2 上传三步曲编排 ---------------------------------------------------

    /// 标记路径为秒传命中（下一次对该路径 precreate 返回 return_type=2）。
    pub fn mark_instant(&self, path: &str) {
        self.state
            .lock()
            .unwrap()
            .instant_paths
            .insert(path.to_string());
    }

    /// 注入索引传播延迟：下一次 method=meta 顶 -9（一次性；见
    /// [`MockState::fail_next_meta`]——真网 2026-09-08 实证形态）。
    ///
    /// 真网 31300/31023 实证后（2026-09-08 第四轮返工）驱动已不调用
    /// meta——本注入面暂无消费者，保留为正式 appkey 复测时的恢复面。
    pub fn fail_next_meta(&self) {
        self.state.lock().unwrap().fail_next_meta = true;
    }

    /// 注入会话死亡：该 uploadid 的**下一分片**返回非 0 error_code
    /// （一次性消费；码值 [`MOCK_DEAD_SESSION_ERROR_CODE`]）。
    pub fn inject_upload_death(&self, uploadid: &str) {
        self.state
            .lock()
            .unwrap()
            .dead_uploads
            .insert(uploadid.to_string(), MOCK_DEAD_SESSION_ERROR_CODE);
    }

    /// 已签发 uploadid 列表（precreate 顺序；create form uploadid 对账用）。
    pub fn uploadids(&self) -> Vec<String> {
        (1..=self.state.lock().unwrap().next_uploadid)
            .map(|n| format!("mock-upload-{n}"))
            .collect()
    }

    // -- B2 下载链路编排 -----------------------------------------------------

    /// 推进 mock 时钟（秒）——超过 Location 的 expires 后旧链 CDN 403
    /// （与 access_token 无关，重取 dlink 才可恢复）。
    pub fn advance_clock(&self, secs: i64) {
        self.state.lock().unwrap().mock_clock += secs;
    }

    /// 切换 CDN 授权形态（两态建模，spike §5）。
    pub fn set_cdn_mode(&self, mode: CdnAuthMode) {
        self.state.lock().unwrap().cdn_mode = mode;
    }

    // -- 观测访问器 ----------------------------------------------------------

    /// 请求记录快照（按到达序）。
    pub fn recorded(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().recorded.clone()
    }

    pub fn refresh_count(&self) -> u64 {
        self.state.lock().unwrap().refresh_count
    }

    /// 当前有效 token 对（oauth 刷新后即轮换值）。
    pub fn current_tokens(&self) -> (String, String) {
        let st = self.state.lock().unwrap();
        (st.access_token.clone(), st.refresh_token.clone())
    }

    /// superfile2 累计收到的分片净荷字节数（conformance ⑦观测点）。
    pub fn bytes_received_total(&self) -> u64 {
        self.state.lock().unwrap().bytes_received_total
    }

    /// 树中 path 以 `<prefix>` 开头的条目数（冲突副本观测面——后端冲突
    /// 重命名形态 `<原名>_<时间戳>`，对 `<父>/<原名>_` 前缀计数即可检出
    /// ghost 副本；`tests/dir_create_preflight.rs` 消费，2026-09-08 第五轮
    /// 真网实证驱动）。
    pub fn entry_count_with_prefix(&self, prefix: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.path.starts_with(prefix))
            .count()
    }

    /// 树条目全路径快照（通用树投影——测试侧自行做形态过滤，如 ghost
    /// 后缀 `_<8位日期>_<6位时间>` 的泛化扫描）。
    pub fn entry_paths(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| e.path.clone())
            .collect()
    }

    /// superfile2 分片请求解析视图（按到达序；差集/表单断言用）。
    pub fn superfile2_records(&self) -> Vec<Superfile2Record> {
        let st = self.state.lock().unwrap();
        st.recorded
            .iter()
            .filter(|r| r.http_method == "POST" && r.path == PCS_SUPERFILE2)
            .filter_map(|r| {
                let pairs = parse_urlencoded(&r.query);
                let qp = |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
                let part = parse_multipart_part(r.content_type.as_deref(), &r.raw_body)?;
                Some(Superfile2Record {
                    uploadid: qp("uploadid")?,
                    partseq: qp("partseq")?.parse().ok()?,
                    part_bytes: part.data.len(),
                    field_name: part.name,
                    part_mime: part.mime,
                    user_agent: r.user_agent.clone(),
                })
            })
            .collect()
    }

    /// 指定 uploadid 会话已收到的 partseq 集合（差集断言的会话视角）。
    pub fn session_partseqs(&self, uploadid: &str) -> Vec<i64> {
        self.state
            .lock()
            .unwrap()
            .sessions
            .get(uploadid)
            .map(|s| s.parts.keys().copied().collect())
            .unwrap_or_default()
    }

    /// 构造指向本 mock 的驱动参数（api/oauth/pcs base 均注入 mock URL；
    /// sessions_dir/dlink_ttl 走缺省 None）。
    pub fn params(&self, token_store: Option<Arc<dyn TokenStore>>) -> BaiduParams {
        self.params_with(token_store, None, None)
    }

    /// [`Self::params`] 的 B2 全参形态（K7 会话表根 / K8 dlink TTL 注入）。
    pub fn params_with(
        &self,
        token_store: Option<Arc<dyn TokenStore>>,
        sessions_dir: Option<std::path::PathBuf>,
        dlink_ttl_secs: Option<u64>,
    ) -> BaiduParams {
        let (access, refresh) = self.current_tokens();
        BaiduParams {
            app_key: MOCK_APP_KEY.to_string(),
            app_secret: MOCK_APP_SECRET.to_string(),
            access_token: Some(access),
            refresh_token: Some(refresh),
            root: MOCK_ROOT.to_string(),
            api_base: self.base_url.clone(),
            oauth_base: self.base_url.clone(),
            token_store,
            sessions_dir,
            dlink_ttl_secs,
            pcs_base: Some(self.base_url.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// axum 处理器
// ---------------------------------------------------------------------------

type Shared = Arc<Mutex<MockState>>;

fn recorded_request(
    http_method: &str,
    path: &str,
    raw_query: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> RecordedRequest {
    RecordedRequest {
        http_method: http_method.to_string(),
        path: path.to_string(),
        query: raw_query.to_string(),
        body: String::from_utf8_lossy(body).into_owned(),
        raw_body: body.to_vec(),
        content_type: headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        user_agent: headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        range_header: headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    }
}

/// 业务端点公共闸门：记录 → 注入顶错 → token 校验（110）。`Some` = 短路响应。
fn business_gate(
    state: &Shared,
    http_method: &str,
    path: &str,
    raw_query: &str,
    pairs: &[(String, String)],
    headers: &HeaderMap,
    body: &Bytes,
) -> Option<Response> {
    let mut st = state.lock().unwrap();
    st.recorded.push(recorded_request(
        http_method,
        path,
        raw_query,
        headers,
        body,
    ));
    if let Some(errno) = st.inject.pop_front() {
        return Some(Json(json!({"errno": errno, "errmsg": "injected"})).into_response());
    }
    let current = st.access_token.clone();
    drop(st);
    let token_ok = pairs
        .iter()
        .any(|(k, v)| k == "access_token" && v == &current);
    if !token_ok {
        return Some(Json(json!({"errno": 110, "errmsg": "invalid access token"})).into_response());
    }
    None
}

fn errno_json(errno: i64) -> Response {
    Json(json!({"errno": errno})).into_response()
}

fn parent_of(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

/// unix 秒 → 真实后端冲突副本后缀形态 `%Y%m%d_%H%M%S`（真网实证样本
/// `20260908_212145`；UTC——测试不钉具体值，形态对齐即可）。日期换算 =
/// civil_from_days（Howard Hinnant 算法），mock 不引 chrono 依赖。
fn conflict_suffix(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let sod = unix_secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}_{:02}{:02}{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

fn entry_json(e: &MockEntry) -> Value {
    json!({
        "fs_id": e.fs_id,
        "path": e.path,
        "server_filename": e.server_filename,
        "size": e.size,
        "isdir": if e.isdir { 1 } else { 0 },
        "md5": e.md5,
        "server_mtime": e.server_mtime,
    })
}

/// `/rest/2.0/xpan/file`：query `method` 分发（list/meta/quota/create/filemanager）。
async fn xpan_file(
    State(state): State<Shared>,
    method: Method,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let raw_query = raw.unwrap_or_default();
    let pairs = parse_urlencoded(&raw_query);
    if let Some(resp) = business_gate(
        &state,
        method.as_str(),
        XPAN_FILE,
        &raw_query,
        &pairs,
        &headers,
        &body,
    ) {
        return resp;
    }
    let qp = |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
    match (method.as_str(), qp("method").unwrap_or_default().as_str()) {
        ("GET", "list") => {
            let dir = qp("dir").unwrap_or_default();
            let st = state.lock().unwrap();
            if !st.entries.iter().any(|e| e.isdir && e.path == dir) {
                return errno_json(-9); // 目录不存在
            }
            let list: Vec<Value> = st
                .entries
                .iter()
                .filter(|e| parent_of(&e.path) == Some(dir.as_str()))
                .map(entry_json)
                .collect();
            Json(json!({"errno": 0, "list": list})).into_response()
        }
        ("GET", "meta") => {
            // 真网 31300/31023 实证（2026-09-08 第四轮返工）：此 appkey 下
            // meta 端点全废，驱动已不调用（stat/Entry 构造/句柄解析全转
            // list）——本路由臂成为**无消费者**，保留原样（历史 wire 断言
            // 形态 + 正式 appkey 复测 meta 权限时的恢复面；fail_next_meta
            // 注入面同理保留）。
            {
                let mut st = state.lock().unwrap();
                if st.fail_next_meta {
                    st.fail_next_meta = false;
                    return errno_json(-9);
                }
            }
            let st = state.lock().unwrap();
            let found = if let Some(fs_ids) = qp("fs_ids") {
                // 形态 "[<id>,…]"（PCFS api.go:176-179）
                let inner = fs_ids.trim_start_matches('[').trim_end_matches(']');
                inner.split(',').find_map(|tok| {
                    let id: i64 = tok.trim().parse().ok()?;
                    st.entries.iter().find(|e| e.fs_id == id)
                })
            } else {
                let p = qp("path").unwrap_or_default();
                st.entries.iter().find(|e| e.path == p)
            };
            match found {
                Some(e) => Json(json!({"errno": 0, "list": [entry_json(e)]})).into_response(),
                None => Json(json!({"errno": -9, "list": []})).into_response(),
            }
        }
        ("GET", "quota") => {
            let st = state.lock().unwrap();
            Json(json!({"errno": 0, "used": st.quota_used, "total": st.quota_total}))
                .into_response()
        }
        ("POST", "create") => {
            let form = parse_urlencoded(&String::from_utf8_lossy(&body));
            let fp = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
            let (Some(path), Some(isdir)) = (fp("path"), fp("isdir")) else {
                return errno_json(-7);
            };
            if isdir == "0" {
                // B2 文件腿（三步曲收尾）：分片齐全性校验（缺片/分片 md5
                // 不符 errno=10，spike §3.3 实证形态）→ 组装入树（rtype=3
                // 覆盖语义）。
                return create_file_finish(&state, &form, &path);
            }
            let mut st = state.lock().unwrap();
            if st.entries.iter().any(|e| e.path == path) {
                // 真网实证（2026-09-08 第五轮，干净探针 netdisk UA）：目录
                // create 撞已存在 ≠ -8——返回 **errno=0**（成功假象），远端
                // 保留原目录并生成 `<原名>_<时间戳>` 空副本目录（实证样本
                // `20260908_212145`）。原 -8 建模废除；驱动侧 mkdir /
                // ensure_parents 转 list 预检（不 create 已存在层），
                // tests/dir_create_preflight.rs 钉死该预检的离线检出力。
                let fs_id = st.next_fs_id;
                st.next_fs_id += 1;
                let now = st.mock_clock;
                let parent = parent_of(&path).unwrap_or("/").to_string();
                let name = path.rsplit('/').next().unwrap_or_default();
                let mut ts = now;
                let ghost_path = loop {
                    let candidate = format!("{parent}/{name}_{}", conflict_suffix(ts));
                    if !st.entries.iter().any(|e| e.path == candidate) {
                        break candidate;
                    }
                    ts += 1; // 同秒重复冲突：推进一秒保副本唯一（防御形态）
                };
                st.entries.push(MockEntry {
                    server_filename: ghost_path
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                    path: ghost_path,
                    isdir: true,
                    size: 0,
                    fs_id,
                    server_mtime: now,
                    md5: String::new(),
                });
                return Json(json!({"errno": 0, "fs_id": fs_id})).into_response();
            }
            if !st
                .entries
                .iter()
                .any(|e| e.isdir && Some(e.path.as_str()) == parent_of(&path))
            {
                return errno_json(-7); // mock 严格语义：驱动须逐级隐式建父目录
            }
            let fs_id = st.next_fs_id;
            st.next_fs_id += 1;
            st.entries.push(MockEntry {
                server_filename: path.rsplit('/').next().unwrap_or_default().to_string(),
                path,
                isdir: true,
                size: 0,
                fs_id,
                server_mtime: 1757000000,
                md5: String::new(),
            });
            Json(json!({"errno": 0, "fs_id": fs_id})).into_response()
        }
        ("POST", "filemanager") => {
            let opera = qp("opera").unwrap_or_default();
            let form = parse_urlencoded(&String::from_utf8_lossy(&body));
            let filelist = form
                .iter()
                .find(|(a, _)| a == "filelist")
                .map(|(_, b)| b.clone())
                .unwrap_or_default();
            let Ok(items) = serde_json::from_str::<Vec<Value>>(&filelist) else {
                return errno_json(-7);
            };
            let mut st = state.lock().unwrap();
            match opera.as_str() {
                "delete" => {
                    let mut info = Vec::new();
                    for item in &items {
                        let p = item
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        match st.entries.iter().position(|e| e.path == p) {
                            Some(i) => {
                                st.entries.remove(i);
                                info.push(json!({"errno": 0, "path": p}));
                            }
                            None => info.push(json!({"errno": -9, "path": p})),
                        }
                    }
                    Json(json!({"errno": 0, "info": info})).into_response()
                }
                "move" => {
                    let mut info = Vec::new();
                    for item in &items {
                        let p = item
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let dest = item
                            .get("dest")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let newname = item
                            .get("newname")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        match st.entries.iter_mut().find(|e| e.path == p) {
                            Some(e) => {
                                e.path = format!("{dest}/{newname}");
                                e.server_filename = newname;
                                info.push(json!({"errno": 0, "path": p}));
                            }
                            None => info.push(json!({"errno": -9, "path": p})),
                        }
                    }
                    // async=1 形态：响应带 taskid；驱动不轮询（两源一致）
                    Json(json!({"errno": 0, "taskid": 1, "info": info})).into_response()
                }
                _ => errno_json(-7),
            }
        }
        // B2：precreate（三步曲第一步；uploadid 签发 + 秒传腿）。
        ("POST", "precreate") => {
            let form = parse_urlencoded(&String::from_utf8_lossy(&body));
            let fp = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
            let Some(path) = fp("path") else {
                return errno_json(-7);
            };
            let size: i64 = fp("size").and_then(|v| v.parse().ok()).unwrap_or(-1);
            if size < 0 {
                return errno_json(-7);
            }
            // block_list 形态 ["<md5hex>",...]（JSON 数组字符串）
            let block_md5: Vec<String> = fp("block_list")
                .and_then(|v| serde_json::from_str::<Vec<String>>(&v).ok())
                .unwrap_or_default();
            // 服务端真形建模（2026-09-23 主会话活 token 真网实测）：空数组
            // block_list 恒拒 errno=2（0 字节明文首跑 ×5 重试降级的缺陷
            // 现场）——0 字节必须声明 `[EMPTY_MD5]`（空串 MD5）。
            if block_md5.is_empty() {
                return errno_json(2);
            }
            let mut st = state.lock().unwrap();
            // 秒传腿：return_type=2 + fs_id 直接收尾（树内生成哨兵内容条目
            // ——秒传语义=云端已有同内容对象，内容不来自本次上传）。
            if st.instant_paths.contains(&path) {
                let fs_id = st.next_fs_id;
                st.next_fs_id += 1;
                let now = st.mock_clock;
                st.entries.retain(|e| e.path != path); // rtype=3 覆盖
                st.entries.push(MockEntry {
                    server_filename: path.rsplit('/').next().unwrap_or_default().to_string(),
                    path: path.clone(),
                    isdir: false,
                    size,
                    fs_id,
                    server_mtime: now,
                    md5: format!("{fs_id:032x}"),
                });
                st.file_blobs.insert(fs_id, pattern_bytes(size as usize));
                return Json(json!({
                    "errno": 0, "return_type": 2, "fs_id": fs_id,
                    "uploadid": "", "block_list": [],
                }))
                .into_response();
            }
            // 正常腿：新会话 + 全量待传索引（重 precreate 不恢复——spike
            // §3.2 实证：同参重发返回新 uploadid + 全量列表）。
            st.next_uploadid += 1;
            let uploadid = format!("mock-upload-{}", st.next_uploadid);
            let pending: Vec<i64> = (0..block_md5.len() as i64).collect();
            st.sessions.insert(
                uploadid.clone(),
                UploadSession {
                    path,
                    size,
                    block_md5,
                    parts: BTreeMap::new(),
                },
            );
            Json(json!({
                "errno": 0, "return_type": 1, "uploadid": uploadid,
                "block_list": pending,
            }))
            .into_response()
        }
        // B2：download dlink 签发（spike §5：禁重定向看 302 Location）。
        ("GET", "download") => {
            let p = qp("path").unwrap_or_default();
            let st = state.lock().unwrap();
            let Some(entry) = st.entries.iter().find(|e| !e.isdir && e.path == p) else {
                return errno_json(-9);
            };
            let fs_id = entry.fs_id;
            let expires = st.mock_clock + MOCK_DLINK_TTL_SECS;
            let location = format!("{}/cdn/{fs_id}?expires={expires}", st.base_url);
            (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
        }
        _ => errno_json(-7),
    }
}

/// `/rest/2.0/xpan/nas?method=uinfo`——uid 来源（VolumeId 构造）。
async fn xpan_nas(
    State(state): State<Shared>,
    method: Method,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let raw_query = raw.unwrap_or_default();
    let pairs = parse_urlencoded(&raw_query);
    if let Some(resp) = business_gate(
        &state,
        method.as_str(),
        XPAN_NAS,
        &raw_query,
        &pairs,
        &headers,
        &body,
    ) {
        return resp;
    }
    let uid = state.lock().unwrap().uid;
    // 建模对齐真实（B2 真机实抓 2026-09-08）：uinfo 响应键为 `uk`
    // （用户标识），无 `uid` 字段；其余键按真实形态裁剪（baidu_name/
    // netdisk_name 等，驱动不解析）。
    Json(json!({"errno": 0, "uk": uid, "baidu_name": "mockuser"})).into_response()
}

// ---------------------------------------------------------------------------
// B2 上传三步曲：create 文件腿 / superfile2 / CDN
// ---------------------------------------------------------------------------

/// `method=create` 的 isdir=0 腿：会话分片齐全性校验（errno=10 族）→
/// 组装文件入树（rtype=3 覆盖语义：同名旧条目移除换新 fs_id）。
fn create_file_finish(state: &Shared, form: &[(String, String)], path: &str) -> Response {
    let fp = |k: &str| {
        form.iter()
            .find(|(a, _)| a == k)
            .map(|(_, b)| b.clone())
            .unwrap_or_default()
    };
    let size: i64 = fp("size").parse().unwrap_or(-1);
    let uploadid = fp("uploadid");
    // create form 的 block_list 与 precreate 同形态（md5 hex 数组字符串）。
    let Ok(blocks) = serde_json::from_str::<Vec<String>>(&fp("block_list")) else {
        return errno_json(-7);
    };
    let mut st = state.lock().unwrap();
    let Some(session) = st.sessions.get_mut(&uploadid) else {
        // 未知 uploadid：与缺片同族（服务端按会话校验，spike §3.3）。
        return errno_json(10);
    };
    // 真网 31363 建模（2026-09-08 干净探针实证）：precreate 一次性锁定
    // (path,size,block_list)，create 的 block_list 必须与 precreate 会话
    // 锁定的声明**原样一致**——不一致 → errno=31363。此即推翻「流式部分
    // 声明」策略的分歧点（create 带全量列表 ≠ precreate 部分声明 → 拒）。
    if blocks != session.block_md5 {
        return errno_json(31363);
    }
    // 齐全性 + 逐片 md5 比对（分片索引 0..n 齐且内容 md5 与声明一致）。
    // 服务端真形例外（2026-09-23 真网实测）：0 字节声明的唯一块
    // `[EMPTY_MD5]` 免分片校验——无 superfile2 可传，precreate 声明即
    // 视为在位（满块恒 4MiB、尾块恒 1..4MiB-1，空串 MD5 只能出自 0 字节）。
    let zero_byte_only = blocks.len() == 1 && blocks[0] == EMPTY_MD5;
    if !zero_byte_only {
        for (idx, want) in blocks.iter().enumerate() {
            match session.parts.get(&(idx as i64)) {
                Some(part) if md5_hex(part) == *want => {}
                _ => return errno_json(10), // 缺片（spike §3.3 实证）/ 分片内容不符
            }
        }
    }
    if session.size != size || session.path != path {
        return errno_json(10); // 会话参数不匹配（mock 严格语义）
    }
    let mut content = Vec::with_capacity(size.max(0) as usize);
    for idx in 0..blocks.len() as i64 {
        // 0 字节免传块无 parts 记录（上面已免校验）——贡献零字节。
        if let Some(part) = session.parts.get(&idx) {
            content.extend_from_slice(part);
        }
    }
    // rtype=3 覆盖：同名条目移除（新 fs_id）+ 旧 blob 清理；K10 语义——
    // 绝不生成 `_2026…` 冲突重命名副本（rtype=1 行为，mock 不建模）。
    let fs_id = st.next_fs_id;
    st.next_fs_id += 1;
    let now = st.mock_clock;
    let old_fs_ids: Vec<i64> = st
        .entries
        .iter()
        .filter(|e| e.path == path)
        .map(|e| e.fs_id)
        .collect();
    st.entries.retain(|e| e.path != path);
    for old in old_fs_ids {
        st.file_blobs.remove(&old);
    }
    st.entries.push(MockEntry {
        server_filename: path.rsplit('/').next().unwrap_or_default().to_string(),
        path: path.to_string(),
        isdir: false,
        size: content.len() as i64,
        fs_id,
        server_mtime: now,
        md5: md5_hex(&content),
    });
    st.file_blobs.insert(fs_id, content);
    // 会话记录保留（B2 绿阶段裁决）：`session_partseqs` 是差集断言的
    // 会话视角观测面（upload_resume 在 close 之后断言会话分片全集
    // [0,1,2]——红阶段 writer 恒 Unsupported、该断言路径从未执行，
    // 「create 即移除会话」的内部簿记与断言自相矛盾，故去除）。对 wire
    // 响应序列零影响——服务端会话终结语义对驱动无可见行为差异。
    Json(json!({"errno": 0, "fs_id": fs_id})).into_response()
}

/// `POST /rest/2.0/pcs/superfile2`——分片上传（PCS 域；响应 **error_code**
/// 而非 errno，spike api.rs:302-311 实证形态）。
///
/// 请求经记录后依次过：会话死亡注入（一次性）→ token 校验 → multipart
/// 解析 → 会话分片累积（`bytes_received_total` 计数）。
async fn superfile2(
    State(state): State<Shared>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let raw_query = raw.unwrap_or_default();
    {
        let mut st = state.lock().unwrap();
        st.recorded.push(recorded_request(
            "POST",
            PCS_SUPERFILE2,
            &raw_query,
            &headers,
            &body,
        ));
    }
    let pairs = parse_urlencoded(&raw_query);
    let qp = |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
    let uploadid = qp("uploadid").unwrap_or_default();
    // 会话死亡注入：该 uploadid 的下一分片顶非 0 error_code（一次性）。
    if let Some(code) = state.lock().unwrap().dead_uploads.remove(&uploadid) {
        return Json(json!({
            "error_code": code, "error_msg": "mock injected session death"
        }))
        .into_response();
    }
    // token 校验（对齐 superfile2 响应形态：错误经 error_code 承载）。
    let current = state.lock().unwrap().access_token.clone();
    if qp("access_token").as_deref() != Some(current.as_str()) {
        return Json(json!({
            "error_code": 110, "error_msg": "Invalid access token"
        }))
        .into_response();
    }
    let Some(partseq) = qp("partseq").and_then(|v| v.parse::<i64>().ok()) else {
        return Json(json!({"error_code": -7, "error_msg": "missing partseq"})).into_response();
    };
    let Some(part) = parse_multipart_part(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        &body,
    ) else {
        return Json(json!({"error_code": -7, "error_msg": "bad multipart"})).into_response();
    };
    let mut st = state.lock().unwrap();
    let part_len = part.data.len();
    let md5 = {
        let Some(session) = st.sessions.get_mut(&uploadid) else {
            // 未知 uploadid（mock 建模码 91002——驱动探活只判非 0，不绑码值）。
            return Json(json!({
                "error_code": 91002, "error_msg": "mock: unknown uploadid"
            }))
            .into_response();
        };
        let md5 = md5_hex(&part.data);
        session.parts.insert(partseq, part.data);
        md5
    };
    st.bytes_received_total += part_len as u64;
    Json(json!({"error_code": 0, "md5": md5})).into_response()
}

/// `GET /cdn/{fs_id}`——CDN 下载。下载三约束（spike §5 dl-try 矩阵）：
/// netdisk 族 UA + 有界 Range ≤4MiB（缺失/开放/超界/全量 → 403 31326）；
/// `expires` 过期（mock 时钟推进）→ 403 且追 token 不可救（重取 dlink）；
/// [`CdnAuthMode::TokenRequired`] 建模追加 access_token 两态。
async fn cdn_get(
    State(state): State<Shared>,
    Path(fs_id): Path<i64>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let raw_query = raw.unwrap_or_default();
    let path = format!("{CDN_PREFIX}{fs_id}");
    {
        let mut st = state.lock().unwrap();
        st.recorded.push(recorded_request(
            "GET",
            &path,
            &raw_query,
            &headers,
            &Bytes::new(),
        ));
    }
    let unauthorized = || {
        (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error_code": 31326,
                "error_msg": "user is not authorized hitcode:104"
            })),
        )
            .into_response()
    };
    // 约束①：netdisk 族 UA。
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !ua.starts_with("netdisk") {
        return unauthorized();
    }
    // 约束②：有界 Range ≤4MiB（无 Range=全量 GET、开放区间、超界均拒）。
    let Some(range) = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_bounded_range)
    else {
        return unauthorized();
    };
    let (start, mut end) = range;
    if end - start + 1 > CHUNK_4M as i64 {
        return unauthorized();
    }
    let pairs = parse_urlencoded(&raw_query);
    let qp = |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
    // 约束③：expires 过期（与 access_token 无关——token 救不了过期链）。
    let st = state.lock().unwrap();
    let expires: i64 = qp("expires").and_then(|v| v.parse().ok()).unwrap_or(0);
    if expires <= st.mock_clock {
        drop(st);
        return unauthorized();
    }
    // 授权两态（spike §5：直连与「追加 access_token」都出现过）。
    if st.cdn_mode == CdnAuthMode::TokenRequired
        && qp("access_token").as_deref() != Some(st.access_token.as_str())
    {
        drop(st);
        return unauthorized();
    }
    let Some(content) = st.file_blobs.get(&fs_id) else {
        return (StatusCode::NOT_FOUND, "no such blob").into_response();
    };
    let total = content.len() as i64;
    if start >= total {
        return (StatusCode::RANGE_NOT_SATISFIABLE, "start beyond eof").into_response();
    }
    end = end.min(total - 1);
    let slice = &content[start as usize..=(end as usize)];
    (
        StatusCode::PARTIAL_CONTENT,
        [
            (
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{total}"),
            ),
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
        ],
        slice.to_vec(),
    )
        .into_response()
}

/// 解析有界 `bytes=<start>-<end>` Range 头（开放区间 `0-` → None——
/// spike §5 实证开放区间 403）。
fn parse_bounded_range(header: &str) -> Option<(i64, i64)> {
    let spec = header.strip_prefix("bytes=")?;
    let (s, e) = spec.split_once('-')?;
    if e.is_empty() {
        return None;
    }
    let start: i64 = s.parse().ok()?;
    let end: i64 = e.parse().ok()?;
    (start <= end).then_some((start, end))
}

/// `/oauth/2.0/token`——refresh 端点（一次一换轮换模型）。
async fn oauth_token(
    State(state): State<Shared>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let raw_query = raw.unwrap_or_default();
    let pairs = parse_urlencoded(&raw_query);
    state.lock().unwrap().recorded.push(recorded_request(
        "GET",
        OAUTH_TOKEN,
        &raw_query,
        &headers,
        &body,
    ));
    let qp = |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
    if qp("grant_type").as_deref() != Some("refresh_token") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unsupported_grant_type", "error_description": "mock supports refresh_token only"})),
        )
            .into_response();
    }
    let mut st = state.lock().unwrap();
    if qp("refresh_token").as_deref() != Some(st.refresh_token.as_str()) {
        // 陈旧 refresh_token：一次一换下旧值即刻作废（spike §1 实证形态）
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant", "error_description": "Invalid Refresh Token"})),
        )
            .into_response();
    }
    st.refresh_count += 1;
    let n = st.refresh_count;
    let access = format!("mock-access-{n}");
    let refresh = format!("mock-refresh-{n}");
    st.access_token = access.clone();
    st.refresh_token = refresh.clone();
    Json(json!({"access_token": access, "refresh_token": refresh, "expires_in": 2592000, "scope": "basic"}))
        .into_response()
}

// ---------------------------------------------------------------------------
// 解析与断言助手（字节级断言的落点）
// ---------------------------------------------------------------------------

/// 解析 `application/x-www-form-urlencoded` 形态串（query 与 form body 同
/// 规则：`%XX` 解码 + `+`→空格；reqwest `.query()`/`.form()` 均按此序列化）。
pub fn parse_urlencoded(pairs: &str) -> Vec<(String, String)> {
    pairs
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (decode_component(k), decode_component(v)),
            None => (decode_component(pair), String::new()),
        })
        .collect()
}

fn decode_component(s: &str) -> String {
    fn hex_val(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push((hi << 4) | lo);
                    i += 3;
                }
                _ => {
                    out.push(b[i]);
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 断言参数集**恰为**期望集（无多余、无缺失、无重复）——字节级断言的
/// 精确集合语义（「恰 method=list&dir=…&access_token=… 三参数」）。
pub fn assert_exact_pairs(actual: &[(String, String)], expected: &[(&str, &str)]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "参数个数不符（应恰为 {expected:?}）：{actual:?}"
    );
    for (k, v) in expected {
        let hits = actual.iter().filter(|(ak, av)| ak == k && av == v).count();
        assert_eq!(hits, 1, "参数 {k}={v} 应恰出现一次：{actual:?}");
    }
}

/// 按 (HTTP 方法, 路径, raw query 子串) 过滤请求记录。
///
/// 子串匹配仅用于 `method=list` / `opera=delete` 等 ASCII 安全值；参数
/// 值断言一律经 [`parse_urlencoded`] 解码后精确匹配。
pub fn filter_recorded<'a>(
    recorded: &'a [RecordedRequest],
    http_method: &str,
    path: &str,
    raw_query_contains: &[&str],
) -> Vec<&'a RecordedRequest> {
    recorded
        .iter()
        .filter(|r| {
            r.http_method == http_method
                && r.path == path
                && raw_query_contains.iter().all(|frag| r.query.contains(frag))
        })
        .collect()
}

/// 断言请求体为 application/x-www-form-urlencoded（表单操作的编码形态）。
pub fn assert_form_encoded(req: &RecordedRequest) {
    assert!(
        req.content_type
            .as_deref()
            .unwrap_or_default()
            .starts_with("application/x-www-form-urlencoded"),
        "应为 form 编码请求：{:?}",
        req.content_type
    );
}

// ---------------------------------------------------------------------------
// B2 共享断言/数据助手
// ---------------------------------------------------------------------------

/// 解析后的 multipart part（superfile2 断言用：字段名/mime/净荷）。
#[derive(Debug, Clone)]
pub struct MultipartPart {
    /// Content-Disposition 的 name（黄金参照 `file`）。
    pub name: String,
    /// Content-Disposition 的 filename（spike 形态 "file"；驱动侧自由度，
    /// 测试不钉）。
    pub file_name: Option<String>,
    pub mime: Option<String>,
    pub data: Vec<u8>,
}

/// 解析单 part multipart/form-data 体（superfile2 契约：恰一个 file part；
/// reqwest `multipart::Form::part("file", …)` 与 PCFS CreateFormFile 同构）。
pub fn parse_multipart_part(content_type: Option<&str>, body: &[u8]) -> Option<MultipartPart> {
    let ct = content_type?;
    if !ct.starts_with("multipart/form-data") {
        return None;
    }
    let boundary = ct
        .split("boundary=")
        .nth(1)?
        .split(';')
        .next()?
        .trim()
        .trim_matches('"');
    let delim = format!("--{boundary}");
    let rest = body.strip_prefix(delim.as_bytes())?.strip_prefix(b"\r\n")?;
    let header_end = find_sub(rest, b"\r\n\r\n")?;
    let header_block = String::from_utf8_lossy(&rest[..header_end]).into_owned();
    let data_start = header_end + 4;
    let closing = format!("\r\n--{boundary}");
    let data_end = find_sub(&rest[data_start..], closing.as_bytes())
        .map(|i| data_start + i)
        .unwrap_or(rest.len());
    let mut name = None;
    let mut file_name = None;
    let mut mime = None;
    for line in header_block.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("content-disposition:") {
            for attr in line.split(';') {
                let attr = attr.trim();
                if let Some(v) = attr.strip_prefix("name=") {
                    name = Some(v.trim_matches('"').to_string());
                } else if let Some(v) = attr.strip_prefix("filename=") {
                    file_name = Some(v.trim_matches('"').to_string());
                }
            }
        } else if lower.starts_with("content-type:") {
            mime = Some(
                line.split_once(':')
                    .map(|(_, v)| v.trim().to_string())
                    .unwrap_or_default(),
            );
        }
    }
    Some(MultipartPart {
        name: name?,
        file_name,
        mime,
        data: rest[data_start..data_end].to_vec(),
    })
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// MD5 hex（分片 md5 对账：precreate block_list ↔ superfile2 净荷）。
pub fn md5_hex(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 确定性伪随机字节（非全零/非递增，防驱动偷懒匹配；形态同 conformance
/// kit 的 pattern）。
pub fn pattern_bytes(n: usize) -> Vec<u8> {
    let mut x = 0x2Fu8;
    (0..n)
        .map(|i| {
            // wrapping：i as u8 + 7 在 i>248 时溢出（debug panic）——大尺寸
            // 数据（4MiB 分片族）必经；i≤248 时与朴素形态值恒等（零漂移）。
            x = x.wrapping_mul(31).wrapping_add((i as u8).wrapping_add(7));
            x
        })
        .collect()
}

/// CDN 请求记录（路径前缀 `/cdn/`；各 fs_id 不同故用前缀过滤）。
pub fn cdn_requests(recorded: &[RecordedRequest]) -> Vec<&RecordedRequest> {
    recorded
        .iter()
        .filter(|r| r.path.starts_with(CDN_PREFIX))
        .collect()
}

// ---------------------------------------------------------------------------
// TokenStore 录制桩（oauth 持久化断言的观测面）
// ---------------------------------------------------------------------------

/// 记录 save_tokens 调用序列的 TokenStore（K13 持久化断言用）。
#[derive(Default)]
pub struct RecordingTokenStore {
    calls: Mutex<Vec<(String, String)>>,
}

impl RecordingTokenStore {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl TokenStore for RecordingTokenStore {
    fn save_tokens(&self, access_token: &str, refresh_token: &str) {
        self.calls
            .lock()
            .unwrap()
            .push((access_token.to_string(), refresh_token.to_string()));
    }
}
