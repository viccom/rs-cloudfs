# SF4 真机矩阵：本地 OpenSSH SFTP fixture（WSL2）

> 用途：`crates/drivers/ck-sftp/tests/live_matrix.rs` 的真机腿夹具。
> **凭据红线（R3）**：测试凭据只经环境变量传给测试进程，绝不入本文件、
> 代码或日志。本文件只记录**搭建步骤与形态**。

## 服务器（WSL2 Ubuntu-24.04，本机）

```bash
# 一次性安装（fixture 脚本见仓库外 E:\tmp\sftp-setup.sh，内容等价）
apt-get install -y openssh-server
useradd -m -s /bin/bash cydrivetest
echo 'cydrivetest:<TEST-ONLY-PASSWORD>' | chpasswd     # 测试密码：env 传入测试进程
mkdir -p /srv/sftp-test && chown cydrivetest:cydrivetest /srv/sftp-test
cat > /etc/ssh/sshd_config.d/99-sftp-test.conf <<EOF
PasswordAuthentication yes
PermitRootLogin no
Subsystem sftp /usr/lib/openssh/sftp-server
UsePAM yes
EOF
ssh-keygen -A
/usr/sbin/sshd -p 2222          # 2222 避开 Windows 侧可能的 22
```

主机指纹（**每次 WSL 重建/ssh-keygen -A 会变**，禁止硬编码进仓库）：

```bash
ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub
# -> 256 SHA256:<...> root@<host> (ED25519)
```

## 符号链接 fixture（⑥ 腿的前置，服务器侧一次性搭建）

```bash
cd /srv/sftp-test
# ⑥a：link-to-dir 可枚举性（修复前会列出 /etc 的 207 个条目）
rm -rf link_test && mkdir link_test
echo REAL_DATA > link_test/real.bin
ln -s real.bin link_test/link.bin
ln -s /etc     link_test/dirlink
chown -R cydrivetest:cydrivetest link_test

# ⑥b：递归删除不下潜（victim 被删、protector 完好 = 通过的判据）
rm -rf link_guard && mkdir -p link_guard/victim
echo guard > link_guard/victim/x.bin
ln -s /srv/sftp-test/protector link_guard/dirlink
mkdir -p protector && echo KEEP > protector/keep.bin
chown -R cydrivetest:cydrivetest link_guard protector
```

`link_guard` 被 ⑥b 测试删除后需**重建**（脚本可重复执行）。

## 运行

```bash
# Git Bash（MSYS 会把 /srv/... 改写成 C:/Program Files/Git/srv/...，
# 必须 MSYS_NO_PATHCONV=1——真机腿第一次跑就踩到，见跟踪单批次日志）
MSYS_NO_PATHCONV=1 \
CYDRIVE_SFTP_TEST_HOST=127.0.0.1 \
CYDRIVE_SFTP_TEST_PORT=2222 \
CYDRIVE_SFTP_TEST_USER=cydrivetest \
CYDRIVE_SFTP_TEST_PASSWORD='<密码>' \
CYDRIVE_SFTP_TEST_FINGERPRINT='SHA256:<指纹>' \
CYDRIVE_SFTP_TEST_ROOT='/srv/sftp-test' \
cargo test -p ck-sftp --test live_matrix -- --ignored --test-threads=1 --nocapture
```

**凭据两形态（D1）二选一**：密码用 `CYDRIVE_SFTP_TEST_PASSWORD`；私钥用
`CYDRIVE_SFTP_TEST_KEY_PATH='<本机未加密私钥路径>'`（如
`C:\Users\<you>\.ssh\id_rsa`）替代 PASSWORD 行。指纹取法：
`ssh-keygen -F <host> -l`（known_hosts 已有记录时）或
`ssh-keyscan <host> | ssh-keygen -lf -`。

## 第二轮：u18 真机（2026-09-15，Phase 4 合入后）

负责人指名的第二台真机：**u18**（`172.27.199.30`，Ubuntu 24.04 OpenSSH，
**root + 本机 RSA 私钥**——`CYDRIVE_SFTP_TEST_KEY_PATH` 腿首次真机验证）。
fixture 建在 `/root/cydrive-sf4`（link_test/link_guard/protector 同上节）；
指纹经 `ssh-keygen -F 172.27.199.30 -l` 取自 known_hosts（ED25519，
**不入仓库**）。递归删除腿消费 link_guard 后需重建（同上节脚本）。

| 项 | 结果 |
|---|---|
| 结果 | **11/11 通过（6.87s）** |
| 上传 128 MiB | 0.96s（133.5 MiB/s） |
| 下载 128 MiB | 2.09s（61.3 MiB/s） |

与 WSL2 回环同档——u18 链路无可见带宽税；D2 三态与符号链接双腿在第二台
服务器复验通过。

## 实测数字（2026-09-15，WSL2 本机回环）

| 项 | 结果 |
|---|---|
| 上传 128 MiB | 0.97s（131.7 MiB/s） |
| 下载 128 MiB | 1.95s（65.6 MiB/s） |
| 结果 | 11/11 通过 |

下载约 66 MiB/s 是**单连接**（russh-sftp 3.0 会话内 `max_concurrent_reads: 16`
流水线读）在 WSL2 回环上的成绩；SF5（文件内分段多连接）的裁决依据
见跟踪单 SF5 行。
