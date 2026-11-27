//! 发布包签名。
//!
//! 实现在 `mesh-install`：启动器也要验清单签名，而它不能依赖本 crate——那条线
//! 会把 ureq、serde、rand、httpdate 全拖进一个只需要 verifier 的二进制。
//! 这里保留 `mesh_remote::signing::*` 这个路径，调用点不必跟着搬家改一遍。

pub use mesh_install::signing::*;
