# cydrive-sync-server 部署文档

> 适用版本：0.5.x ｜ 对应客户端：cydrive ≥ 0.5.0（`sync_url` 配置键）

## 1. 这是什么

`cydrive-sync-server` 是家庭级**元数据同步服务端**：一个单二进制 + 一个 SQLite 文件，提供两个 JSON 端点（`POST /v1/push`、`POST /v1/pull`）。多台机器上的 cydrive 客户端经由它共享同一份盘索引（文件名/大小/分块消息号等**元数据**；文件字节本身始终在 Telegram，不经过本服务）。

- **一致性模型**：每命名空间单调版本计数 + LWW（后推者胜）+ 墓碑删除，最终一致。
- **命名空间**：`hex(SHA-256(bot_token + ":" + chat_id))`——**服务器永不接触、永不存储裸 bot token**，客户端只在请求体里携带派生键。同一 bot+chat 的所有机器共享同一个盘；不同 bot 各自独立。
- **设计边界（家庭级，刻意不做）**：无用户系统、无配额、无内建 TLS（交反代）、拉取不鉴权（见 §5）。

## 2. 获取二进制

在任一有 Rust 工具链的 Linux 机器上（或 WSL）：

```bash
git clone <本仓库> && cd rs-CyDrive
cargo build --release -p cydrive-sync
# 产物：target/release/cydrive-sync-server
```

零运行时依赖（SQLite 已静态编入）。Windows 同理产出 `.exe`。

## 3. 配置（全部走环境变量，无配置文件）

| 变量 | 默认 | 说明 |
|---|---|---|
| `SYNC_LISTEN` | `127.0.0.1:8290` | 监听地址。`0.0.0.0:8290` = 对局域网开放（配合防火墙/反代） |
| `SYNC_DB` | `./cydrive_sync.db`（相对 cwd） | SQLite 数据库路径 |
| `SYNC_SECRET` | 无 | 可选共享密钥。**设置后 push 与 pull 都必须在请求体携带匹配值**（客户端侧用 config.toml 的 `sync_secret` 或环境变量 `CYDRIVE_SYNC_SECRET`）。不设则两端点开放（仅限内网/隧道形态） |

命令行：无参数启动；`--version` / `-V` 打版本；`--help` / `-h` 打用法；**任何其他参数直接拒绝退出（exit 2），不会启动服务**。

## 4. systemd 部署（推荐，Linux 服务器）

仓库自带 unit：`deploy/cydrive-sync.service`。完整步骤（root）：

```bash
# 1. 安装二进制
cp target/release/cydrive-sync-server /usr/local/bin/

# 2. 可选共享密钥（强烈建议公网部署时设置）
install -m 600 /dev/null /etc/cydrive-sync.env
echo 'SYNC_SECRET=挑一个足够长的随机串' > /etc/cydrive-sync.env

# 3. 安装并启动（unit 已配 StateDirectory、SYNC_DB、RUST_LOG=info、
#    Restart=on-failure + 5s 间隔；密钥经 EnvironmentFile 注入）
cp deploy/cydrive-sync.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now cydrive-sync

# 4. 验证
systemctl status cydrive-sync        # 应为 active (running)
journalctl -u cydrive-sync -f        # 应见 "listening on http://127.0.0.1:8290"
```

Windows 上跑（可选形态）：`SYNC_LISTEN=0.0.0.0:8290 SYNC_DB=D:\cydrive\sync.db cydrive-sync-server.exe`，用计划任务/服务包装器驻留，防火墙放行端口。

## 5. TLS 与暴露面（公网部署必读）

服务端**不内建 TLS**，设计形态是挂在反代后面：

```caddy
# Caddy（自动签发 Let's Encrypt 公网证书）
sync.example.com {
    reverse_proxy 127.0.0.1:8290
}
```

```nginx
# nginx 最小示例（证书自备）
server {
    listen 443 ssl;
    server_name sync.example.com;
    ssl_certificate     /etc/letsencrypt/live/sync.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/sync.example.com/privkey.pem;
    location / {
        proxy_pass http://127.0.0.1:8290;
        # 反代侧建议加并发/限速（服务端单请求 body 上限 64MB）
    }
}
```

客户端的 https 校验基于 **Mozilla 公共根证书**（webpki-roots）：

- ✅ 公网 CA 签发的证书（Let's Encrypt 等）直接可用；
- ❌ **自签证书不受信**——需要加密通道但无公网域名时，改用隧道（WireGuard / SSH 隧道 / Tailscale）跑内网 http，或仅 LAN 直连 `SYNC_LISTEN`。

**安全模型**（0.6.0 起）：`SYNC_SECRET` 同时拦截 **push 与 pull**——密钥未配或不对一律 403。不设密钥 = 两端点对任何能到达端口的人开放（仅建议内网/隧道形态）。公网部署务必设置密钥。

> **升级顺序（0.6.0 是协议变更）**：新客户端对旧服务端（≤0.5.2）完全兼容（旧端忽略新字段）；**旧客户端（≤0.5.2）对新服务端的 pull 会被 403 拒**（旧 pull 请求不带密钥字段）。所以：先升级各客户端，再升级服务端并设置 `SYNC_SECRET`。

## 6. 客户端接线（各台机器）

```toml
# config.toml（cydrive 工作目录）
sync_url = "https://sync.example.com"   # 不写或留空 = 同步功能关闭
sync_interval_secs = 300                 # 可选，默认 300，范围 1..=86400
```

共享密钥两种给法（优先级：环境变量 > config.toml）：

```toml
# config.toml（推荐，最省事）
sync_secret = "与服务端一致的密钥"
```

```bash
# 或环境变量（适合 systemd/容器场景）
export CYDRIVE_SYNC_SECRET='与服务端一致的密钥'
```

随后 `cydrive run` 启动即同步一轮 + 每周期自动同步；`cydrive sync` 手动触发一次。空盘新机器（换机/新装）跑一次 sync 即可拿到完整索引，盘里立刻能列出全部文件（字节仍按需从 Telegram 下载）。

**同盘条件**：所有需要共享的机器使用**相同的 bot_token + chat_id**（派生出同一命名空间）。

## 7. 运维

**备份**：数据库就一个文件。停服拷贝，或在线快照：

```bash
sqlite3 /var/lib/cydrive-sync/cydrive_sync.db ".backup '/root/sync_backup.db'"
```

**升级**：替换二进制 → `systemctl restart cydrive-sync`。schema 变更均为幂等增量（`IF NOT EXISTS`），升级不动老数据。

**⚠️ 数据库重建后的恢复流程**（删库重开/换库文件时必做）：服务端版本计数从零重来，而客户端镜像里记着旧的高版本号——**不重置客户端的话，新数据会被幂等闸挡住，永不生效**。在每台客户端的 cydrive 工作目录执行：

```bash
sqlite3 cydrive_meta.db "DELETE FROM sync_mirror; DELETE FROM sync_state;"
```

然后各机器跑一次 `cydrive sync`（会把本地全量重新推上服务端，LWW 自动收敛）。

**验证端点**（服务端机上）：

```bash
# 未知命名空间：应返回 {"rows":[],"max_version":0}
curl -s -X POST http://127.0.0.1:8290/v1/pull \
  -H 'Content-Type: application/json' -d '{"key":"probe","since":0}'

# 配了 SYNC_SECRET 时，不带密钥 push 应 403 {"error":"..."}
curl -s -X POST http://127.0.0.1:8290/v1/push \
  -H 'Content-Type: application/json' \
  -d '{"key":"probe","secret":"wrong","rows":[]}'
```

## 8. 故障速查

| 现象 | 处置 |
|---|---|
| journal 无 `listening on` 行 | unit 已设 `RUST_LOG=info`；手动跑时需自设 `RUST_LOG=info`（tracing 缺省只出 ERROR） |
| 客户端报 `scheme is not http` | sync_url 写了 https 但走的是 0.5.0 客户端（不支持 TLS）；升级 ≥0.5.1 |
| 客户端 403 | `CYDRIVE_SYNC_SECRET` 未设或与服务端 `SYNC_SECRET` 不一致 |
| 空盘 sync 后仍空 | 两机器 bot_token/chat_id 不一致（不同命名空间）；核对 config |
| 重建 db 后同步"失灵" | 见 §7 恢复流程，重置客户端 sync 两表 |
