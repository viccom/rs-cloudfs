//! 驱动能力位（foundation D3 九种 + Batch B3b 第十位 `remote_delete`）。
//!
//! **R4 红线：能力位必须诚实**——只声明经过 conformance kit（离线套件）
//! 与真机验证的能力（local 类驱动豁免真机项）。消费方启动时按能力探测
//! 降级（无 INBOUND → 入站 worker 不启动 + 日志声明），**绝不 panic**。

/// 十个能力位的集合形态。
///
/// 形态裁决：bool 字段 struct 而非 bitflags——零新依赖、每bit 独立文档
/// 注释承载 conformance 含义（R4 的「诚实」是逐位的）；判等/子集比较
/// 经 [`Capabilities::contains`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// `reader(range)` 支持半开区间读取（断言②全套：精确窗口/越界钳制/
    /// start>=size 声明一致）。未声明的驱动只接受 `range = None` 整读。
    pub range_read: bool,
    /// 断点续传：上传中途丢弃 stager 后重传同路径只补差集（断言⑦，
    /// 驱动层可观测）。
    pub resume: bool,
    /// 后端原生分块上传（telegram 1900MB parts / baidu 4MB superfile）——
    /// 分块策略归驱动，上层不可见。
    pub multipart: bool,
    /// rename 是后端单侧操作（O(1) move，无重传）。未声明时 rename 允许
    /// 降级为 copy+delete（存在中间可见态）。
    pub server_side_move: bool,
    /// 秒传（由内容指纹直接落盘）——配套可选 trait
    /// [`crate::optional::RapidUpload`] 消费 [`crate::vocab::WriteHint`]。
    pub rapid_upload: bool,
    /// 权威索引：后端 list 即真相（baidu/115/123/local）→ 新机器
    /// bootstrap = 列目录重建 db；telegram 为影子索引（D4）。
    pub authoritative_index: bool,
    /// 后端变更推送——配套可选 trait [`crate::optional::ChangeFeed`]。
    pub change_feed: bool,
    /// 入站通道（bot 收文件等）。无此能力的实例不启动入站 worker。
    pub inbound: bool,
    /// 对话通道（bot 对话式交互）。
    pub chat: bool,
    /// 远端对象删除原语（Batch B3b / K4，第 10 位）：
    /// [`crate::transport::CloudTransport::delete_remote`] 可真实删除后端
    /// 对象（local 按 path / baidu 按 fs_id）。**K4 依据**：权威索引后端
    /// 删除若只删本地行会造成云端孤儿 + rebuild 复活——driver-onboarding
    /// §9 勿抄清单明令禁止；消费方（Vfs/webdav/web 删除路径，B3b 段二
    /// 接线）按本位门控：真 → 先删远端（幂等重试）成功后再删本地行+缓存，
    /// 失败中止并保留行；假 → 现行为（Python parity：远端对象保留）。
    /// telegram/mock 声明 false（行为零变化）。该位描述 transport 面的
    /// 删除原语；[`crate::driver::StorageDriver::delete`] 的「真删后端」
    /// 语义（9 位时代已如此）不因此位改变。
    pub remote_delete: bool,
}

impl Capabilities {
    /// 全部能力位为 false（起步形态，逐位点亮）。
    pub fn none() -> Self {
        Capabilities::default()
    }

    /// 子集判定：`self` 是否包含 `other` 声明的全部能力位
    /// （`a.contains(&b)` ⇔ b ⊆ a；`contains(&self)` 恒真，即自反）。
    pub fn contains(&self, other: &Capabilities) -> bool {
        let Capabilities {
            range_read,
            resume,
            multipart,
            server_side_move,
            rapid_upload,
            authoritative_index,
            change_feed,
            inbound,
            chat,
            remote_delete,
        } = *other;
        self.range_read >= range_read
            && self.resume >= resume
            && self.multipart >= multipart
            && self.server_side_move >= server_side_move
            && self.rapid_upload >= rapid_upload
            && self.authoritative_index >= authoritative_index
            && self.change_feed >= change_feed
            && self.inbound >= inbound
            && self.chat >= chat
            && self.remote_delete >= remote_delete
    }

    /// 是否一个能力位都没有声明。
    pub fn is_empty(&self) -> bool {
        *self == Capabilities::none()
    }
}
