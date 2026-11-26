//! 开机自启：任务计划程序里的一条 `Mesh Client` 任务。
//!
//! Windows 上客户端必须以管理员身份运行才能建 TUN，写 `HKCU\...\Run` 的话每次开机都要
//! 用户点一次 UAC，等于没自启。改用任务计划程序的「登录时运行 + 最高权限」，系统直接
//! 给提权令牌，不弹框。
//!
//! 其它平台不提供：Linux 上客户端本来就是 systemd 的 `ExecStart`，自启归服务管理器管，
//! 客户端再插一手只会和它打架。
//!
//! 状态以系统里有没有这条任务为准，客户端自己不记：任务可能被用户或组策略删掉，
//! 内存里存一份“上次设置成什么”只会和真相对不上。查询要起一个 `schtasks` 进程，
//! 所以结果缓存在引擎里，只在启动、用户改动和每轮心跳之后刷新。

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{disable, enable, enabled, SUPPORTED};

#[cfg(not(windows))]
mod other;
#[cfg(not(windows))]
pub(crate) use other::{disable, enable, enabled, SUPPORTED};
