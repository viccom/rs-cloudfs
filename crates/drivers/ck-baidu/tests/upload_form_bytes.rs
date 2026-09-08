//! 上传三步曲表单**字节级断言**（Batch B2 红①）。
//!
//! 断言收到的 query/form/multipart 恰为黄金参照（解析为参数集后做精确
//! 集合断言：无多余、无缺失、无重复）。逐操作注源：
//!
//! - **precreate**：spike `examples/baidu_spike/src/api.rs:187-234` 实抓 +
//!   PCFS api.go:488-493——form 恰六字段
//!   `path,size,isdir=0,autoinit=1,rtype,block_list`；**rtype=3**（K10 覆盖
//!   语义；spike 用 1 是冲突重命名，本驱动改 3，真机 B2 复核）；
//! - **superfile2**：spike api.rs:270-320——query 恰六参数
//!   `method=upload,access_token,path,type=tmpfile,uploadid,partseq`，
//!   multipart 字段名 `file`（octet-stream），净荷 = 对应 4MiB 分片字节；
//! - **create**：spike api.rs:332-370 + PCFS api.go:581-587——form 恰六字段
//!   `path,size,isdir=0,rtype,uploadid,block_list`；
//! - **block_list** 形态：`["<md5hex>",...]`——分片 MD5 按 **4MiB 边界对
//!   内容**计算（测试逐片对账），非全文件 MD5。

mod common;

use std::sync::Arc;

use ck_baidu::{factory, BaiduDriver};
use cloudkit_storage::{EntryKind, RelPath, StorageDriver, WriteHint};

use common::{
    assert_exact_pairs, assert_form_encoded, filter_recorded, md5_hex, parse_multipart_part,
    parse_urlencoded, pattern_bytes, MockBaidu, CHUNK_4M, INITIAL_ACCESS_TOKEN, MOCK_ROOT,
    PCS_SUPERFILE2, XPAN_FILE,
};

/// 种子根目录 + 构造驱动（sessions_dir=None：本套件只看 wire 形态）。
async fn setup() -> (MockBaidu, Arc<BaiduDriver>) {
    let (mock, _base) = MockBaidu::start().await;
    mock.seed_dir(MOCK_ROOT);
    let driver = factory(&mock.params(None))
        .await
        .expect("baidu driver connect（uinfo 取 uid）");
    (mock, driver)
}

/// 全量上传 helper：3 分片数据（2×4MiB + 100 字节）→ close → Entry。
async fn upload_3_blocks(driver: &BaiduDriver, rel: &str) -> Vec<u8> {
    let data = pattern_bytes(2 * CHUNK_4M + 100);
    let path = RelPath::new(rel).expect("rel path");
    let hint = WriteHint {
        size: Some(data.len() as u64),
        ..Default::default()
    };
    let mut stager = driver
        .writer(&path, &hint)
        .await
        .expect("writer 打开（B2 契约：三步曲 stager）");
    stager.write(&data).await.expect("write staging 数据");
    let entry = stager.close().await.expect("close 提交三步曲");
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.size, data.len() as u64);
    data
}

/// 期望的 block_list JSON 字符串（3 分片：4MiB+4MiB+100B，按内容 md5）。
fn expected_block_list_json(data: &[u8]) -> String {
    let blocks = [
        md5_hex(&data[..CHUNK_4M]),
        md5_hex(&data[CHUNK_4M..2 * CHUNK_4M]),
        md5_hex(&data[2 * CHUNK_4M..]),
    ];
    serde_json::to_string(&blocks).expect("block_list JSON 序列化")
}

#[tokio::test]
async fn precreate_posts_exact_six_field_form_with_rtype3_and_content_md5_blocks() {
    let (mock, driver) = setup().await;
    let data = upload_3_blocks(&driver, "f3.bin").await;

    let recorded = mock.recorded();
    let reqs = filter_recorded(&recorded, "POST", XPAN_FILE, &["method=precreate"]);
    assert_eq!(reqs.len(), 1, "precreate 恰一次后端调用");
    // query 恰两参数（PCFS api.go:483：?method=precreate&access_token=…）。
    assert_exact_pairs(
        &parse_urlencoded(&reqs[0].query),
        &[
            ("method", "precreate"),
            ("access_token", INITIAL_ACCESS_TOKEN),
        ],
    );
    assert_form_encoded(reqs[0]);
    // 黄金参照（PCFS api.go:488-493 + spike api.rs:200-207）：form 恰六字段。
    // rtype=3（K10 覆盖语义——spike 的 rtype=1 是冲突重命名，明确否定）。
    let form = parse_urlencoded(&reqs[0].body);
    let path = format!("{MOCK_ROOT}/f3.bin");
    assert_exact_pairs(
        &form,
        &[
            ("path", path.as_str()),
            ("size", "8388708"), // 2*4MiB+100
            ("isdir", "0"),
            ("autoinit", "1"),
            ("rtype", "3"),
            ("block_list", expected_block_list_json(&data).as_str()),
        ],
    );
}

#[tokio::test]
async fn superfile2_posts_exact_six_param_query_with_multipart_file_part_per_4mib_slice() {
    let (mock, driver) = setup().await;
    let data = upload_3_blocks(&driver, "f3.bin").await;

    let records = mock.superfile2_records();
    assert_eq!(records.len(), 3, "3 分片数据 → 恰 3 次 superfile2 调用");

    // uploadid 与 precreate 签发值一致（mock 计数器形态）。
    let uploadids = mock.uploadids();
    assert_eq!(uploadids.len(), 1, "恰一次 precreate 签发会话");
    let uploadid = &uploadids[0];
    let path = format!("{MOCK_ROOT}/f3.bin");

    let mut partseqs: Vec<i64> = records.iter().map(|r| r.partseq).collect();
    partseqs.sort();
    assert_eq!(partseqs, vec![0, 1, 2], "分片索引恰 0/1/2（4MiB 边界切分）");

    for record in &records {
        // 黄金参照（spike api.rs:286-294）：query 恰六参数。
        let recorded = mock.recorded();
        let reqs: Vec<_> = recorded
            .iter()
            .filter(|r| r.http_method == "POST" && r.path == PCS_SUPERFILE2)
            .filter(|r| r.query.contains(&format!("partseq={}", record.partseq)))
            .collect();
        assert_eq!(reqs.len(), 1, "partseq={} 恰一次调用", record.partseq);
        assert_exact_pairs(
            &parse_urlencoded(&reqs[0].query),
            &[
                ("method", "upload"),
                ("access_token", INITIAL_ACCESS_TOKEN),
                ("path", path.as_str()),
                ("type", "tmpfile"),
                ("uploadid", uploadid.as_str()),
                ("partseq", &record.partseq.to_string()),
            ],
        );
        // multipart：字段名 file + octet-stream（任务协议参数；filename 是
        // 驱动侧自由度，不钉）。
        assert_eq!(
            record.field_name, "file",
            "multipart 字段名必须为 file（spike api.rs:283）"
        );
        assert_eq!(
            record.part_mime.as_deref(),
            Some("application/octet-stream"),
            "multipart part MIME 为 octet-stream"
        );
    }

    // 净荷逐片对账：partseq=i 的字节恰 = data 的第 i 个 4MiB 切片
    // （superfile2 走 stream client——记录经 raw_body 保真解析）。
    let slices = [
        &data[..CHUNK_4M],
        &data[CHUNK_4M..2 * CHUNK_4M],
        &data[2 * CHUNK_4M..],
    ];
    for (i, slice) in slices.iter().enumerate() {
        let rec = records
            .iter()
            .find(|r| r.partseq == i as i64)
            .expect("分片记录存在");
        assert_eq!(
            rec.part_bytes,
            slice.len(),
            "partseq={i} 净荷字节数 = 4MiB 边界切片长度"
        );
        // 逐字节对账：从 raw 记录重新解析 multipart 净荷。
        let recorded = mock.recorded();
        let reqs: Vec<_> = recorded
            .iter()
            .filter(|r| r.http_method == "POST" && r.path == PCS_SUPERFILE2)
            .filter(|r| r.query.contains(&format!("partseq={i}")))
            .collect();
        let part = parse_multipart_part(reqs[0].content_type.as_deref(), &reqs[0].raw_body)
            .expect("multipart 可解析");
        assert_eq!(&part.data, *slice, "partseq={i} 净荷逐字节等于对应切片");
    }
}

#[tokio::test]
async fn create_posts_exact_six_field_form_with_precreate_uploadid_and_blocks() {
    let (mock, driver) = setup().await;
    let data = upload_3_blocks(&driver, "f3.bin").await;

    let recorded = mock.recorded();
    // create（isdir=0 腿）与 mkdir 的 create（isdir=1）以 body 区分。
    let reqs: Vec<_> = filter_recorded(&recorded, "POST", XPAN_FILE, &["method=create"])
        .into_iter()
        .filter(|r| r.body.contains("isdir=0"))
        .collect();
    assert_eq!(reqs.len(), 1, "create（文件腿）恰一次后端调用");
    // query 恰两参数（PCFS api.go:577 形态）。
    assert_exact_pairs(
        &parse_urlencoded(&reqs[0].query),
        &[("method", "create"), ("access_token", INITIAL_ACCESS_TOKEN)],
    );
    assert_form_encoded(reqs[0]);
    // 黄金参照（PCFS api.go:581-587 + spike api.rs:345-353）：form 恰六字段；
    // uploadid 必须来自 precreate 签发的同一会话（差集续传的会话锚点）。
    let uploadids = mock.uploadids();
    assert_eq!(uploadids.len(), 1, "恰一次 precreate（无中断重传）");
    let form = parse_urlencoded(&reqs[0].body);
    let path = format!("{MOCK_ROOT}/f3.bin");
    assert_exact_pairs(
        &form,
        &[
            ("path", path.as_str()),
            ("size", "8388708"),
            ("isdir", "0"),
            ("rtype", "3"),
            ("uploadid", uploadids[0].as_str()),
            ("block_list", expected_block_list_json(&data).as_str()),
        ],
    );
}

#[tokio::test]
async fn empty_upload_uses_empty_block_list_without_superfile2() {
    let (mock, driver) = setup().await;
    // 空文件：block_list=[]（0 分片）、superfile2 零调用、create 直接收尾
    // （conformance ① n=0 腿的 wire 形态钉死）。
    let path = RelPath::new("empty.bin").expect("rel path");
    let hint = WriteHint {
        size: Some(0),
        ..Default::default()
    };
    let mut stager = driver
        .writer(&path, &hint)
        .await
        .expect("writer 打开（空文件腿）");
    stager.write(&[]).await.expect("write 空数据");
    let entry = stager.close().await.expect("close 空文件三步曲");
    assert_eq!(entry.size, 0, "空文件 Entry.size=0");

    assert_eq!(
        mock.superfile2_records().len(),
        0,
        "0 分片 → superfile2 零调用"
    );
    let recorded = mock.recorded();
    let reqs: Vec<_> = filter_recorded(&recorded, "POST", XPAN_FILE, &["method=precreate"]);
    assert_eq!(reqs.len(), 1, "precreate 恰一次");
    let form = parse_urlencoded(&reqs[0].body);
    assert_exact_pairs(
        &form,
        &[
            ("path", format!("{MOCK_ROOT}/empty.bin").as_str()),
            ("size", "0"),
            ("isdir", "0"),
            ("autoinit", "1"),
            ("rtype", "3"),
            ("block_list", "[]"),
        ],
    );
}
