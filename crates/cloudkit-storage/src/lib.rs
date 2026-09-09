//! # cloudkit-storage——存储抽象层（L2）
//!
//! 多云存储统一契约：[`StorageDriver`] trait、公共词汇类型、能力位、
//! 错误分类学与 conformance kit。
//!
//! 层位置与红线：
//! - **L2**：不依赖任何 workspace 内其他 crate、不依赖任何驱动；
//! - **R1**：运行时代码后端无关——不认识任何具体后端的错误码/参数
//!   （百度 errno 三档映射只以 test-only fixture 形态钉在测试里）；
//! - **R2**：所有后端错误在驱动内映射为 [`StorageError`]，L3+ 永远只见它。
//!
//! 模块地图：[`vocab`]（词汇类型）→ [`ids`]（VolumeId/EntryId，D6）→
//! [`capability`]（十能力位 = foundation D3 九位 + B3b `remote_delete`）→
//! [`error`]（分类学，D2）→ [`stager`]
//! （commit-on-close）→ [`driver`]（主 trait，D1）→ [`optional`]（可选
//! trait 骨架）→ [`mock`]（内存后端）→ [`conformance`]（八条断言套件，D9）。
//! 另有 Phase 1 Batch R 迁入的历史接缝家族：[`transport`]（CloudTransport
//! trait 家族 + 契约类型 + MockTransport，R-3/R-4）与 [`vpath`]（其
//! 载荷路径类型）。
//!
//! 注意：[`transport::ByteStream`]（Send+Sync，历史接缝）与
//! [`vocab::ByteStream`]（Send，StorageDriver 家族）是两个类型；transport
//! 模块的符号只在模块路径下暴露（不在 crate root re-export），避免与
//! 词汇类型遮蔽混淆。

pub mod capability;
pub mod conformance;
pub mod driver;
pub mod error;
pub mod ids;
pub mod mock;
pub mod optional;
pub mod stager;
pub mod transport;
pub mod vocab;
pub mod vpath;

pub use capability::Capabilities;
pub use conformance::{assert_conforms, ConformanceHarness, ErrorReplay};
pub use driver::StorageDriver;
pub use error::StorageError;
pub use ids::{BackendHandle, EntryId, VolumeId};
pub use mock::MockStorageDriver;
pub use optional::{ChangeEvent, ChangeFeed, ChangePage, RapidUpload, TokenEvents};
pub use stager::UploadStager;
pub use vocab::{
    ByteStream, Entry, EntryKind, Listing, Page, PageCursor, Quota, Range, RelPath, WriteHint,
};

/// 一行接入 conformance 套件（interfaces §6 / foundation D9）。
///
/// 在驱动的测试模块展开为一个 `#[tokio::test]` 测试函数，跑完整八条
/// 断言（未声明能力位自动跳过对应项）。消费 crate 需自带
/// `tokio`（`rt` + `macros`）dev-dependency。
///
/// ```ignore
/// // ck-local 的 tests/conformance.rs 里只需：
/// struct LocalHarness { driver: ck_local::LocalDriver }
/// # #[async_trait::async_trait]
/// # impl cloudkit_storage::ConformanceHarness for LocalHarness { /* ... */ }
/// cloudkit_storage::conformance_suite!(LocalHarness::new());
/// ```
#[macro_export]
macro_rules! conformance_suite {
    ($harness:expr) => {
        #[tokio::test]
        async fn conformance_suite_offline() {
            $crate::conformance::assert_conforms(&$harness).await;
        }
    };
}
