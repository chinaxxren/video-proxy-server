extern crate lazy_static;

pub mod data_source;
pub mod handlers;
pub mod storage;
pub mod utils;
pub mod data_request;
pub mod data_source_manager;
pub mod server;
pub mod hls;
pub mod request_handler;

#[macro_export]
macro_rules! log_info {
    ($tag:expr, $($arg:tt)*) => {
        println!("[{} INFO {}] {}", 
            chrono::Local::now().format("%H:%M:%S"),
            $tag,
            format!($($arg)*)
        )
    };
}

pub use data_request::DataRequest;
pub use data_source_manager::DataSourceManager;


