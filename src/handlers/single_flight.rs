//! 同一 `(key, 区间)` 的并发回源合并。
//!
//! 播放器卡顿重试、两个播放器共享同一资源、或者播放器自己把一个请求拆成
//! 几路并发，都会产生「同一个 key、同一个区间」的重复请求。合并之前每一路
//! 都各自开一条上游连接、各自尝试写缓存，于是：
//!
//! - 上游流量翻 N 倍，N-1 份是纯浪费；
//! - N 个写入任务抢同一把 key 写锁，抢不到的那些攒满通道后被
//!   [`crate::handlers::tee`] 的宽限期判定为「缓存侧阻塞」而放弃缓存——
//!   下载了却没存下来，是最坏的一种结果。
//!
//! 这里只做**精确区间**合并：`(key, start, requested_end)` 完全相同才算重复。
//! 区间部分重叠的情况不管——那需要区间树和「把一个请求拆成多段分别等不同
//! leader」的逻辑，复杂度和收益不成比例，而播放器产生的重复请求几乎都是
//! 完全相同的区间（重试就是把同一个 Range 再发一遍）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

/// follower 等 leader 的上限。
///
/// 超时不是错误，只是「不等了，自己去拉」——退化成合并之前的行为，
/// 而不是让请求失败。所以这个值可以取得比较宽松。
///
/// 取值权衡：太短则大区间的合并几乎不生效（leader 还没下完，follower 就
/// 全跑去自己拉了）；太长则 leader 一旦卡住（上游慢、客户端不读导致
/// 背压），所有 follower 都跟着卡。30 秒的含义是「leader 都卡了半分钟，
/// 与其继续等，不如自己试一次」。
const FOLLOWER_WAIT_LIMIT: Duration = Duration::from_secs(30);

type Key = (String, u64, u64);

/// 在途回源登记表。
#[derive(Debug, Default)]
pub struct SingleFlight {
    /// 用 `std::sync::Mutex` 而不是 tokio 的：临界区只有一次 HashMap 查改，
    /// 不跨 await，用异步锁反而多一次调度。
    in_flight: Mutex<HashMap<Key, watch::Sender<bool>>>,
}

/// [`SingleFlight::join`] 的结果。
pub enum Join {
    /// 本请求是这个区间的 leader，负责实际回源。
    ///
    /// 守卫必须一直活到**缓存写完**为止，见 [`LeaderGuard`]。
    Leader(LeaderGuard),
    /// 已有 leader 在拉这个区间，本请求等它。
    Follower(Follower),
}

impl SingleFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次回源意图。
    ///
    /// 表里没有这个区间就成为 leader 并登记；已经有了就返回 follower 句柄。
    pub fn join(self: &Arc<Self>, key: &str, start: u64, requested_end: u64) -> Join {
        let map_key: Key = (key.to_string(), start, requested_end);

        // 锁中毒说明别处 panic 过。合并只是优化，降级成「谁都当 leader」
        // 仍然正确（就是合并之前的行为），不该让它把请求打挂。
        let Ok(mut in_flight) = self.in_flight.lock() else {
            return Join::Leader(LeaderGuard {
                registry: None,
                key: map_key,
                done: None,
            });
        };

        if let Some(sender) = in_flight.get(&map_key) {
            // subscribe 拿到的接收者只对**此刻之后**的变更敏感，所以下面
            // follower 里要先看一眼当前值，不能直接 changed()。
            return Join::Follower(Follower {
                done: sender.subscribe(),
            });
        }

        let (sender, _) = watch::channel(false);
        in_flight.insert(map_key.clone(), sender.clone());
        Join::Leader(LeaderGuard {
            registry: Some(self.clone()),
            key: map_key,
            done: Some(sender),
        })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.in_flight.lock().unwrap().len()
    }
}

/// leader 的在途登记凭证。**析构即宣告完成。**
///
/// 用 `Drop` 而不是显式调用，是因为完成的路径太多：回源失败、上游中途报错、
/// 写缓存失败、任务被取消、panic。漏掉任何一条，follower 就会一直等到超时。
/// 交给析构函数是唯一不会漏的写法。
///
/// **这个守卫必须被移动进缓存写入任务，活到 `write_stream` 返回为止。**
/// 如果只活到「响应构造完」，follower 被唤醒时缓存里还没有数据，只能各自
/// 再去回源一次——合并等于没做。
#[derive(Debug)]
pub struct LeaderGuard {
    /// `None` 表示这是锁中毒时的降级守卫，没有登记，也就无需注销。
    registry: Option<Arc<SingleFlight>>,
    key: Key,
    done: Option<watch::Sender<bool>>,
}

impl Drop for LeaderGuard {
    fn drop(&mut self) {
        // 先从表里摘掉，再广播完成。顺序反了会有一个窗口：follower 收到完成
        // 通知后立刻重试，此时表里还留着这条已完成的登记，它会又变成
        // follower 去等一个不存在的 leader，白等一个超时。
        if let Some(registry) = &self.registry {
            if let Ok(mut in_flight) = registry.in_flight.lock() {
                in_flight.remove(&self.key);
            }
        }
        if let Some(done) = &self.done {
            // 发送失败只意味着没有 follower 在等，正常情况。
            let _ = done.send(true);
        }
    }
}

/// follower 的等待句柄。
#[derive(Debug)]
pub struct Follower {
    done: watch::Receiver<bool>,
}

impl Follower {
    /// 等 leader 完成。
    ///
    /// 返回 `true` 表示 leader 已经结束（成功或失败都算结束，follower 接下来
    /// 要自己查缓存来判断拿到了什么）；`false` 表示等到超时，leader 还没完。
    ///
    /// 无论哪种结果，调用方都必须自己有兜底路径：leader 可能失败、可能只写了
    /// 一部分。等待成功**不等于**缓存里就有完整数据。
    pub async fn wait(mut self) -> bool {
        // 先看当前值：subscribe 之后 leader 可能已经析构过了，那次广播
        // 我们没收到。少了这一步，这类 follower 会一直等到超时。
        if *self.done.borrow() {
            return true;
        }

        // `changed()` 自己返回 Err 时也算完成：那说明发送端已经 drop，即
        // leader 走了没来得及 send 的路径（比如 panic）。所以这里只看外层
        // 的 timeout 结果——Ok 表示等到了通知，Err 表示等到了超时。
        tokio::time::timeout(FOLLOWER_WAIT_LIMIT, self.done.changed())
            .await
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_caller_leads_and_second_follows() {
        let flight = Arc::new(SingleFlight::new());

        let Join::Leader(guard) = flight.join("k", 0, 99) else {
            panic!("第一个调用者应当是 leader");
        };
        assert!(matches!(flight.join("k", 0, 99), Join::Follower(_)));

        drop(guard);
        // 登记必须在析构时清掉，否则下一轮请求会去等一个已经结束的 leader。
        assert_eq!(flight.len(), 0);
    }

    #[tokio::test]
    async fn different_ranges_do_not_merge() {
        let flight = Arc::new(SingleFlight::new());
        let _first = flight.join("k", 0, 99);
        // 区间不同就不是重复请求，必须各自回源。
        assert!(matches!(flight.join("k", 100, 199), Join::Leader(_)));
        // key 不同同理。
        assert!(matches!(flight.join("other", 0, 99), Join::Leader(_)));
    }

    #[tokio::test]
    async fn follower_wakes_when_leader_finishes() {
        let flight = Arc::new(SingleFlight::new());
        let Join::Leader(guard) = flight.join("k", 0, 99) else {
            panic!("应当是 leader");
        };
        let Join::Follower(follower) = flight.join("k", 0, 99) else {
            panic!("应当是 follower");
        };

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(guard);
        });

        assert!(
            tokio::time::timeout(Duration::from_secs(5), follower.wait())
                .await
                .expect("follower 没能在 leader 结束后醒来"),
        );
    }

    /// leader 在 follower 调 `wait()` 之前就结束了。
    ///
    /// 这是 `watch` 最容易踩的坑：`changed()` 只对订阅之后的变更敏感，
    /// 不先看一眼当前值的话，这个 follower 会一直等到超时。
    #[tokio::test]
    async fn follower_that_subscribed_late_does_not_hang() {
        let flight = Arc::new(SingleFlight::new());
        let Join::Leader(guard) = flight.join("k", 0, 99) else {
            panic!("应当是 leader");
        };
        let Join::Follower(follower) = flight.join("k", 0, 99) else {
            panic!("应当是 follower");
        };

        drop(guard); // 先结束，follower 还没开始等

        assert!(
            tokio::time::timeout(Duration::from_secs(5), follower.wait())
                .await
                .expect("晚订阅的 follower 卡死了"),
        );
    }

    /// leader panic 时守卫仍会析构，follower 不能被永久挂起。
    #[tokio::test]
    async fn follower_wakes_even_if_leader_panics() {
        let flight = Arc::new(SingleFlight::new());
        let Join::Leader(guard) = flight.join("k", 0, 99) else {
            panic!("应当是 leader");
        };
        let Join::Follower(follower) = flight.join("k", 0, 99) else {
            panic!("应当是 follower");
        };

        let panicking = tokio::spawn(async move {
            let _guard = guard;
            panic!("leader 挂了");
        });
        assert!(panicking.await.is_err());

        assert!(
            tokio::time::timeout(Duration::from_secs(5), follower.wait())
                .await
                .expect("leader panic 后 follower 卡死了"),
        );
        assert_eq!(flight.len(), 0);
    }

    /// leader 结束后同一区间应当能重新开始，而不是永远被当成在途。
    #[tokio::test]
    async fn range_can_be_led_again_after_completion() {
        let flight = Arc::new(SingleFlight::new());
        let Join::Leader(first) = flight.join("k", 0, 99) else {
            panic!("应当是 leader");
        };
        drop(first);
        assert!(matches!(flight.join("k", 0, 99), Join::Leader(_)));
    }
}
