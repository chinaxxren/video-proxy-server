//! 进程唯一的日志出口。
//!
//! 之前这里是一套完整但**没有任何调用方**的 `Logger`/`LogLevel`，而真正在用
//! 的是 `lib.rs` 里另一个直接 `println!` 的 `log_info!` 宏。两套并存的后果是
//! 死掉的那套带着「release 不打日志」的设计，活着的那套什么门都没有。
//!
//! 现在只剩这一套。集中到一个函数还有个后续用途：`println!` 的输出在 iOS 上
//! 不进系统日志、在 Android 上直接被丢弃，将来要接 `oslog`/`android_logger`
//! 只需要改这里一处。

use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// 日志总开关，默认开。
///
/// 用 `Relaxed`：这只是个开关，不需要和其他内存操作建立顺序关系，读到的是
/// 前一个值还是后一个值都无所谓。
static ENABLED: AtomicBool = AtomicBool::new(true);

/// 打开或关闭日志。
///
/// 给库的使用方留的入口——被嵌进 App 时，宿主通常有自己的日志系统，
/// 不希望这里再往 stdout 写东西。
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 按环境变量 `PROXY_LOG` 初始化开关：设为 `0` 时关闭。
pub fn init_from_env() {
    if let Ok(value) = std::env::var("PROXY_LOG") {
        set_enabled(value != "0");
    }
}

/// 写一行 INFO 日志。
///
/// 收 [`fmt::Arguments`] 而不是 `String`，这是这次改动的重点。原先的宏是
/// `println!("...{}", format!($($arg)*))`：先把消息 `format!` 成一个 String，
/// 再让 `println!` 把这个 String 重新格式化一遍写进 stdout。等于每条日志一次
/// 多余的堆分配加一趟多余的格式化，而全仓库有 56 个调用点，几乎每个请求都要
/// 过好几个。
///
/// 关掉时提前返回，此时 `Arguments` 里的内容根本不会被格式化——`format_args!`
/// 只是持有实参的引用，真正的格式化发生在这里写出去的那一刻。
pub fn info(tag: &str, args: fmt::Arguments<'_>) {
    if !is_enabled() {
        return;
    }

    // 显式加锁写一次。`println!` 每次调用本来也要拿这把锁，区别只在于
    // 现在是「格式化直接进 stdout 缓冲」而不是「先进一个临时 String」。
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // 忽略写入错误。`println!` 在写失败时是 panic 的——服务被 daemonize
    // 之后 stdout 可能已经关掉，一条日志不该把进程带走。
    let _ = writeln!(
        out,
        "[{} INFO {}] {}",
        chrono::Local::now().format("%H:%M:%S"),
        tag,
        args
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 开关必须真的挡住输出。这个测试主要是钉住「关掉之后不再走格式化」
    /// 这条路径不会 panic，也不会因为提前返回而漏掉状态恢复。
    #[test]
    fn toggle_controls_output() {
        let original = is_enabled();

        set_enabled(false);
        assert!(!is_enabled());
        // 关闭状态下调用一次：不应该有任何输出，也不应该 panic。
        info("Test", format_args!("这条不该出现 {}", 1));

        set_enabled(true);
        assert!(is_enabled());

        set_enabled(original);
    }
}
