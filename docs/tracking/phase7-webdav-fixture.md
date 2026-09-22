# WD0/WD5 真机矩阵：双 WebDAV 服务器 fixture（WSL2）

> Phase 7（webdav 驱动）真机怪癖 spike 与 WD5 矩阵的服务器侧说明。
> 形态照 `phase4-sftp-fixture.md`；凭据只经 env（R3）；fixture 为本机回环一次性凭据。
> 建立日期：2026-09-21（WD0 批）。矩阵证据 = curl 双轮探针 + `examples/webdav_spike`（Rust 化可重跑）。

## 服务器（WSL2 Ubuntu-24.04，本机）

| 服务器 | 端口 | 认证 | 用途 |
|---|---|---|---|
| rclone serve webdav v1.60.1 | 8080 | Basic（`Www-Authenticate: Basic realm="rclone"`） | 明文 Basic 腿 + 全动词 |
| Apache 2.4.58 mod_dav_fs | 8081 `/dav/` | Digest（realm `webdavtest`，qop=auth，MD5） | Digest 腿 + PROPPATCH 腿 |
| Apache 同站 `/dav-stale/` | 8081 | Digest + `AuthDigestNonceLifetime 2` | stale=true 再协商腿 |

**连接形态（重要）**：本机 Windows→WSL2 的 localhostForwarding 当前失效（127.0.0.1 直连 000）——
Windows 侧一律走 **VM IP**：`wsl.exe -d Ubuntu-24.04 -- bash -c "hostname -I"`（首个 IP；NAT 模式，重启会变）。
WSL 内部则用 127.0.0.1。两腿均实测可用。

**持久性**：apache2 经 systemd（enabled，随 WSL 自启）；rclone 经 nohup + pid 文件 `/run/webdav-spike-rclone.pid`
（WSL 重启后须重跑安装脚本；脚本幂等，rclone 段先杀旧实例）。

## 一次性安装（幂等脚本原文；在 WSL 内以 root 运行）

```bash
# 依赖：apt-get install -y apache2 rclone   # mod_dav/mod_dav_fs 在 apache2 包内
RCLONE_ROOT=/srv/rclone-dav
APACHE_ROOT=/srv/webdav-test
DIGEST_FILE=/etc/apache2/webdav.digest
REALM=webdavtest; SPIKE_USER=spike; SPIKE_PASS=<见 env，勿入库>

mkdir -p "$RCLONE_ROOT" "$APACHE_ROOT/dav" "$APACHE_ROOT/dav-stale"
# htdigest 非交互生成：user:realm:md5(user:realm:pass)
HASH=$(printf '%s:%s:%s' "$SPIKE_USER" "$REALM" "$SPIKE_PASS" | md5sum | cut -d' ' -f1)
printf '%s:%s:%s\n' "$SPIKE_USER" "$REALM" "$HASH" > "$DIGEST_FILE"
chown root:www-data "$DIGEST_FILE"; chmod 640 "$DIGEST_FILE"   # 640 root:root 会 500（实测坑）
chown -R www-data:www-data "$APACHE_ROOT"

a2enmod dav dav_fs auth_digest
cat > /etc/apache2/sites-available/webdav-spike.conf <<'EOF'
Listen 8081
<VirtualHost *:8081>
    DavLockDB /var/lock/apache2/DavLock
    DocumentRoot /srv/webdav-test
    <Location /dav>
        DAV On
        AuthType Digest
        AuthName "webdavtest"
        AuthDigestProvider file
        AuthUserFile /etc/apache2/webdav.digest
        Require valid-user
    </Location>
    <Location /dav-stale>
        DAV On
        AuthDigestNonceLifetime 2
        AuthType Digest
        AuthName "webdavtest"
        AuthDigestProvider file
        AuthUserFile /etc/apache2/webdav.digest
        Require valid-user
    </Location>
    ErrorLog ${APACHE_LOG_DIR}/webdav-spike-error.log
    CustomLog ${APACHE_LOG_DIR}/webdav-spike-access.log combined
</VirtualHost>
EOF
a2ensite webdav-spike && apache2ctl configtest && systemctl restart apache2

nohup rclone serve webdav "$RCLONE_ROOT" --addr 0.0.0.0:8080 \
    --user "$SPIKE_USER" --pass "$SPIKE_PASS" --vfs-cache-mode writes \
    > /tmp/webdav-spike-rclone.log 2>&1 &
echo $! > /run/webdav-spike-rclone.pid
```

探活：`curl -s -o /dev/null -w '%{http_code}\n' -u spike:<pw> -X PROPFIND -H 'Depth: 0' http://127.0.0.1:8080/`（207）
与 `curl -s -D - -o /dev/null -X PROPFIND -H 'Depth: 0' http://127.0.0.1:8081/dav/ | grep WWW-Authenticate`（401 Digest challenge）。

## 怪癖矩阵（WD0 钉死，2026-09-21；curl + webdav_spike 双源交叉）

驱动对策列为 WD2/WD3 设计输入。rclone 版本 1.60.1（Ubuntu 24.04 apt 版）。

| # | 怪癖 | rclone serve webdav | Apache mod_dav | 驱动对策 |
|---|---|---|---|---|
| 1 | PROPPATCH 写 mtime | 207+内层 403（`lastmodified` 与 `getlastmodified` 双形态皆拒；后者含 `cannot-modify-protected-property`） | `lastmodified`→207+内层 200 但**真实 mtime 不变**（存成 dead prop 假成功）；`getlastmodified`→207+内层 409 read-only（内层字面 `HTTP/1.1 409 (status)`，判码不判文案） | **D2 降级：generic 不写 mtime**（读真源 `getlastmodified`；修订见计划 §8-D2） |
| 2 | X-OC-Mtime | 忽略（mtime=上传时刻） | 忽略 | nextcloud vendor 值保留搭车（fixture 无 Nextcloud，未实证、零成本） |
| 3 | chunked PUT | 接受（100 KiB 流式无 Content-Length，201+字节数完整） | 接受 | D5 维持 spool+Content-Length 路线（样本仅二，不升格默认；挂账） |
| 4 | Range 形态 | 206+`Content-Range: bytes a-b/N`；越 EOF **钳制** 206（90-999999→90-99/100）；416+`Content-Range: bytes */N`+短错误体；倒序 50-10→**416** | 206 ✓；钳制 ✓；416 ✓；倒序→**200 全量**（忽略 Range） | 200 截断回退必须实现；416→EOF 语义（offset≥size）；倒序不出现在驱动（半开区间自造） |
| 5 | MOVE | Overwrite:F→412 / T→204；**缺 Overwrite 头+目标存在→412**（偏离 RFC 缺省 T）；目录 MOVE→201；**201 后 VFS 缓存不可见窗**（子项立即 PROPFIND/GET→404/500，`--vfs-cache-mode writes` 下 dir-cache-time 缺省 ≈5min 过期恢复；数据实际已落盘；窗内 DELETE 204 撒谎留残） | F→412（"Destination is not empty"）/ T→204；**目录 no-slash→301 不执行**；源+目标均 slashed→201 "Destination has been created"；目标父缺失→**500**（非 409） | 恒发显式 Overwrite；目录腿源+Destination 均带尾斜杠；Destination 恒绝对 URI；rclone 不可见窗=已知怪癖挂账（rename 后 list 空窗≠数据丢失；真机矩阵断言设计须避开） |
| 6 | MKCOL | **已存在目录→201（幂等成功，非 405！）**；父缺失→409；同名文件→405 | 已存在 slashed→405 / no-slash→301；父缺失→409；同名文件→405 | **mkdir 先 stat 预检再 MKCOL**（rclone 201 陷阱——baidu mkdir 先例同型）；MKCOL 恒带尾斜杠 |
| 7 | DELETE | 文件 204→再删 404；集合递归 204（no-slash 集合也执行） | slashed 集合 204 递归；**no-slash 集合→301 不执行** | 集合 DELETE 恒带尾斜杠 |
| 8 | PROPFIND | Depth 0/1 ✓；空目录=仅 self 条目；**文件+尾斜杠也 207**（尾斜杠全不敏感）；href `%20`+`&amp;`+大写 `%C3%BC`；`D:` 前缀；mtime=IMF-fixdate；目录 `getcontentlength` 在内层 404 propstat | **集合 no-slash→301**（文件 no-slash ✓）；同文档内 `D:`/`ns0:`/`lp1:`/`g0:` 多前缀并存（皆绑 "DAV:"）+`lp2:`=apache.org/dav/props/；creationdate ISO8601；mtime=IMF-fixdate | 集合 PROPFIND 恒带尾斜杠；**解析按 local-name**（前缀不可信）；href 实体反转义+百分号解码；mtime 三格式（IMF-fixdate 用 httpdate） |
| 9 | 认证 challenge | `Www-Authenticate: Basic realm="rclone"`（头名大小写不定） | `Digest realm="webdavtest", nonce="<b64 含 =/+>", algorithm=MD5, qop="auth"`；stale=true **位置不定**（实测在 algorithm 后）；**nc 重放不查**（同 nonce 同 nc 二发仍 207）；nonce 过期→401+stale=true→新 nonce 重算恰一次即恢复 | 引号感知+顺序无关解析；nc 单调自守（客户端正确性）；stale 再协商恰一次（D1） |
| 10 | Destination 相对 URI | 接受（201+移动生效） | **400 拒绝**（移动不发生） | 恒发绝对 URI（url crate 构造） |
| 11 | quota（RFC 4331） | 内层 propstat 404（两键皆无） | 内层 propstat 404 | quota→None 降级（sftp 先例） |

附加观察：apache GET 集合 slashed→403（无 autoindex）、no-slash→301；rclone GET 集合 slashed→**200+HTML 目录页**（serve webdav 自带浏览 UI）、no-slash→405；PUT 到集合 rclone→404、apache slashed→409 "Cannot PUT to a collection"；rclone PROPFIND 不存在→404、apache 同；apache 401 挑战在 GET 上同样出现。
XML 解析器宽容度：rclone 对畸形 XML 声明容忍、apache expat 400（spike 执行期副发现——**构造请求体必须严格良构**）。

## 运行（Rust 化 spike，可重跑）

```bash
# Git Bash（Windows 侧，VM-IP 直连；spike 代码内已 no_proxy，env 保险）
IP=$(wsl.exe -d Ubuntu-24.04 -- bash -c "hostname -I" | tr -d '\r\n\0' | awk '{print $1}')
export WEBDAV_SPIKE_RCLONE_URL="http://$IP:8080/" \
       WEBDAV_SPIKE_APACHE_URL="http://$IP:8081/dav/" \
       WEBDAV_SPIKE_APACHE_STALE_URL="http://$IP:8081/dav-stale/" \
       WEBDAV_SPIKE_USER=spike WEBDAV_SPIKE_PASS=<pw>
cargo run --manifest-path examples/webdav_spike/Cargo.toml --release -- matrix   # 十腿全矩阵
cargo run --manifest-path examples/webdav_spike/Cargo.toml --release -- cleanup  # 前缀扫尾
```

实测（2026-09-21）：matrix 全腿跑通，rclone 1 mismatch（=rclone 目录 MOVE 不可见窗的刻意留证）/apache 0 mismatch；
digest 全链（challenge→207→nc 递增→stale→新 nonce 一次恢复）绿。注意 rclone 缓存窗期间 cleanup 需二轮。

## 维护

- WSL 重启后：apache 自启；rclone 重跑安装脚本（幂等）。
- VM IP 变化：重取 `hostname -I` 更新 env。
- 排错：`/var/log/apache2/webdav-spike-error.log`、`/tmp/webdav-spike-rclone.log`。
- 清根（测试间隔离）：`rm -rf /srv/rclone-dav/* /srv/webdav-test/dav/* /srv/webdav-test/dav-stale/*`（root）。

## WD5 真机矩阵运行（2026-09-22 实测全绿）

```bash
# Git Bash（Windows 侧；spike/live_matrix 代码内已 no_proxy）
IP=$(wsl.exe -d Ubuntu-24.04 -- bash -c "hostname -I" | tr -d '\r\n\0' | awk '{print $1}')
export CYDRIVE_WEBDAV_TEST_RCLONE_URL="http://$IP:8080/" \
       CYDRIVE_WEBDAV_TEST_APACHE_URL="http://$IP:8081/dav/" \
       CYDRIVE_WEBDAV_TEST_APACHE_STALE_URL="http://$IP:8081/dav-stale/" \
       CYDRIVE_WEBDAV_TEST_USER=spike CYDRIVE_WEBDAV_TEST_PASS=<pw>
cargo test -p ck-webdav --test live_matrix -- --ignored --test-threads=1 --nocapture
```

十腿全绿 88.5s（stamp 唯一名 + LCG 随机内容；收尾自动清扫并核空，测试内含腿⑨的 rclone 杀/起与全局残留清扫）。关键数字：吞吐 214.8↑/234.2↓ MiB/s（128 MiB 回环）；digest stale 恢复 1.9ms；断线自愈 kill 75ms→Unavailable 9.6s→重启后首成功 508ms（1 次尝试）；probe 五态真机分类全对（含 rclone OPTIONS 免认证语义——probe 需真认证动词复核，WD5 揭出并修复）。

**wsl 通道脚本纪律（WD5 实证沉淀，测试代码内注释留档）**：Rust `Command` 经 wsl.exe 传多行/含双引号/含变量赋值的脚本会被 argv 重 join/重引号静默变形（曾致「文件静默落到 WSL 根 + wc 断言照常通过」与「`test -d "$D"` 恒假」两种假象）——一律**单行 + 字面路径 + 零引号零变量**，`test -f` 尾守卫 fail loud；多行输出（find 等）不得回填进后续脚本，用 `$(...)` 命令替换单行完成。
