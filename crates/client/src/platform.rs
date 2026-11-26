//! 随平台变化的动作：控制台、消息框、打开浏览器/目录、信号。
//!
//! 只有这些动作走 cfg；线程怎么排、退出顺序、headless 的控制台跟随都是两平台
//! 共用（PLAN.md 第 4 节）。

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
