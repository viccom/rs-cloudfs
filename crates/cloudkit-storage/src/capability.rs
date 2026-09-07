//! 驱动能力位（foundation D3，九种）。
//!
//! **R4 红线：能力位必须诚实**——只声明经过 conformance kit（离线套件）
//! 与真机验证的能力（local 类驱动豁免真机项）。消费方启动时按能力探测
//! 降级（无 INBOUND → 入站 worker 不启动 + 日志声明），**绝不 panic**。

/// 九个能力位的集合形态。
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
}

impl Capabilities {
    /// 全部能力位为 false（起步形态，逐位点亮）。
    pub fn none() -> Self {
        Capabilities::default()
    }

    /// 子集判定：`self` 是否包含 `other` 声明的全部能力位
    /// （`a.contains(&b)` ⇔ b ⊆ a；`contains(&self)` 恒真，即自反）。
    pub fn contains(&self, other: &Capabilities) -> bool {
        // red-commit skeleton: 判定逻辑在绿提交落地
        let _ = (self, other);
        false
    }

    /// 是否一个能力位都没有声明。
    pub fn is_empty(&self) -> bool {
        *self == Capabilities::none()
    }
}
