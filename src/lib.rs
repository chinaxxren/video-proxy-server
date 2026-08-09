#[cfg(feature = "android-jni")]
mod android_jni;
pub mod data_request;
pub mod data_source;
pub mod data_source_manager;
pub mod ffi;
pub mod handlers;
#[cfg(any(feature = "harmony-napi", test))]
mod harmony_config;
#[cfg(feature = "harmony-napi")]
mod harmony_napi;
pub mod hls;
pub mod http_types;
#[cfg(feature = "p2p")]
pub mod p2p;
#[cfg(feature = "p2p-network")]
pub mod p2p_network;
#[cfg(feature = "p2p-librqbit")]
pub mod rqbit_backend;
pub mod request_handler;
pub mod server;
pub mod source_registry;
pub mod storage;
pub mod utils;

/// 写一行 INFO 日志。调用语法与之前完全一致。
///
/// 展开成 `logger::info($tag, format_args!(...))`，而不是原先的
/// `println!("...{}", format!(...))`。差别有两处：消息不再先落成一个临时
/// String（`format_args!` 只持有实参引用，格式化推迟到真正写出去时），
/// 以及日志关闭时整个格式化过程被跳过。
#[macro_export]
macro_rules! log_info {
    ($tag:expr, $($arg:tt)*) => {
        $crate::utils::logger::info($tag, format_args!($($arg)*))
    };
}

pub use data_request::DataRequest;
pub use data_source_manager::DataSourceManager;
