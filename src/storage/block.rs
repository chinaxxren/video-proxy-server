use crate::utils::error::{ProxyError, Result};
use std::cmp::max;
use std::collections::BTreeMap;
use std::ops::Range;
use std::time::SystemTime;
use tokio::sync::RwLock;

/// 区块状态
#[derive(Debug, Clone, PartialEq)]
pub enum BlockState {
    Complete,    // 完成
    Downloading, // 下载中
    Pending,     // 等待下载
}

/// 区块信息
#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub offset: u64,             // 起始位置
    pub length: u64,             // 长度
    pub state: BlockState,       // 状态
    pub last_access: SystemTime, // 最后访问时间
    pub priority: u32,           // 优先级
}

/// 区块管理器
#[derive(Debug)]
pub struct BlockManager {
    blocks: RwLock<BTreeMap<u64, BlockInfo>>, // 使用 BTreeMap 按偏移量排序存储区块
}

impl Default for BlockManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockManager {
    pub fn new() -> Self {
        Self {
            blocks: RwLock::new(BTreeMap::new()),
        }
    }

    /// 返回 `range` 中尚未被「已完成」区块覆盖的部分。
    ///
    /// 只有 `Complete` 区块才算覆盖：`Pending`/`Downloading` 的数据还不能读。
    ///
    /// 原实现对未完成区块也会 push 一段缺失，但不推进 `current`，末尾又无条件
    /// 补一段 `current..range.end`，于是同一片区间被报告两次。例如区块
    /// `50..100` 处于 `Pending`、查询 `0..150` 时会返回 `[0..50, 0..150]`——
    /// 调用方按这个结果去下载就会重复请求并在同一偏移重叠写入。
    /// 现在改为只以 `Complete` 区块为界切分，结果保证有序、互不重叠。
    pub async fn check_range(&self, range: Range<u64>) -> Vec<Range<u64>> {
        if range.start >= range.end {
            return Vec::new();
        }

        let blocks = self.blocks.read().await;
        let mut missing_ranges = Vec::new();
        let mut current = range.start;

        // BTreeMap 按 offset 升序遍历，因此 current 单调递增。
        for (offset, block) in blocks.iter() {
            if block.state != BlockState::Complete {
                continue;
            }

            let block_end = offset.saturating_add(block.length);
            // 完全落在已覆盖部分之前的区块无关紧要。
            if block_end <= current {
                continue;
            }
            // 区块起点已超出查询范围，后面的只会更远。
            if *offset >= range.end {
                break;
            }

            if current < *offset {
                missing_ranges.push(current..*offset);
            }
            current = max(current, block_end);
            if current >= range.end {
                break;
            }
        }

        if current < range.end {
            missing_ranges.push(current..range.end);
        }

        missing_ranges
    }

    /// 添加新区块
    pub async fn add_block(&self, offset: u64, length: u64, state: BlockState) -> Result<()> {
        if length == 0 {
            return Err(ProxyError::Cache("区块长度必须大于 0".to_string()));
        }
        let end = offset
            .checked_add(length)
            .ok_or_else(|| ProxyError::Cache("区块范围溢出".to_string()))?;
        let mut blocks = self.blocks.write().await;

        // 检查是否与现有区块重叠
        if let Some((_, existing)) = blocks.range(..=offset).next_back() {
            if offset < existing.offset + existing.length {
                return Err(ProxyError::Cache("区块重叠".to_string()));
            }
        }
        if let Some((next_offset, _)) = blocks.range(offset..).next() {
            if end > *next_offset {
                return Err(ProxyError::Cache("区块重叠".to_string()));
            }
        }

        blocks.insert(
            offset,
            BlockInfo {
                offset,
                length,
                state,
                last_access: SystemTime::now(),
                priority: 0,
            },
        );

        // 在同一个写锁内合并，避免 Tokio RwLock 的不可重入死锁。
        Self::merge_blocks(&mut blocks);
        Ok(())
    }

    /// 更新区块状态
    pub async fn update_block_state(&self, offset: u64, state: BlockState) -> Result<()> {
        let mut blocks = self.blocks.write().await;
        if let Some(block) = blocks.get_mut(&offset) {
            block.state = state;
            block.last_access = SystemTime::now();
            Ok(())
        } else {
            Err(ProxyError::Cache("区块不存在".to_string()))
        }
    }

    /// 合并相邻区块
    fn merge_blocks(blocks: &mut BTreeMap<u64, BlockInfo>) {
        let mut merged = Vec::new();
        let mut current_block: Option<BlockInfo> = None;

        // 收集需要合并的区块
        for (_, block) in blocks.iter() {
            if let Some(mut current) = current_block {
                if current.offset + current.length == block.offset
                    && current.state == BlockState::Complete
                    && block.state == BlockState::Complete
                {
                    // 可以合并
                    current.length += block.length;
                    current_block = Some(current);
                } else {
                    // 不能合并，保存当前区块
                    merged.push(current);
                    current_block = Some(block.clone());
                }
            } else {
                current_block = Some(block.clone());
            }
        }

        // 保存最后一个区块
        if let Some(block) = current_block {
            merged.push(block);
        }

        // 更新区块表
        blocks.clear();
        for block in merged {
            blocks.insert(block.offset, block);
        }
    }

    /// 获取下一个要下载的区块
    pub async fn get_next_pending_block(&self) -> Option<BlockInfo> {
        let mut blocks = self.blocks.write().await;
        for (_, block) in blocks.iter_mut() {
            if block.state == BlockState::Pending {
                block.state = BlockState::Downloading;
                return Some(block.clone());
            }
        }
        None
    }

    /// 清理过期区块
    pub async fn cleanup_expired_blocks(&self, max_age: std::time::Duration) {
        let mut blocks = self.blocks.write().await;
        let now = SystemTime::now();
        blocks.retain(|_, block| {
            if let Ok(age) = now.duration_since(block.last_access) {
                age < max_age
            } else {
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn adjacent_complete_blocks_are_merged_without_deadlock() {
        let manager = BlockManager::new();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            manager
                .add_block(0, 10, BlockState::Complete)
                .await
                .unwrap();
            manager
                .add_block(10, 5, BlockState::Complete)
                .await
                .unwrap();
        })
        .await
        .expect("add_block deadlocked");

        assert!(manager.check_range(0..15).await.is_empty());
    }

    #[tokio::test]
    async fn rejects_overlap_on_either_side_and_invalid_lengths() {
        let manager = BlockManager::new();
        manager
            .add_block(10, 10, BlockState::Complete)
            .await
            .unwrap();

        assert!(manager
            .add_block(15, 10, BlockState::Complete)
            .await
            .is_err());
        assert!(manager
            .add_block(5, 10, BlockState::Complete)
            .await
            .is_err());
        assert!(manager.add_block(0, 0, BlockState::Complete).await.is_err());
        assert!(manager
            .add_block(u64::MAX, 2, BlockState::Complete)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn reports_only_ranges_not_covered_by_complete_blocks() {
        let manager = BlockManager::new();
        manager.add_block(0, 5, BlockState::Complete).await.unwrap();
        manager.add_block(5, 5, BlockState::Pending).await.unwrap();
        manager
            .add_block(10, 5, BlockState::Complete)
            .await
            .unwrap();

        assert_eq!(manager.check_range(0..20).await, vec![5..10, 15..20]);
        let pending = manager.get_next_pending_block().await.unwrap();
        assert_eq!(pending.offset, 5);
        assert_eq!(pending.state, BlockState::Downloading);
        manager
            .update_block_state(5, BlockState::Complete)
            .await
            .unwrap();
        assert_eq!(manager.check_range(0..15).await, Vec::<Range<u64>>::new());
        assert!(manager
            .update_block_state(99, BlockState::Complete)
            .await
            .is_err());
    }

    /// 未完成区块不得让缺失区间重复或重叠。
    ///
    /// 旧实现在这个用例下返回 `[0..50, 0..150]`：既重复报告 0..50，又把
    /// 已经报告过的区间再包一遍。
    #[tokio::test]
    async fn incomplete_block_inside_range_yields_one_contiguous_gap() {
        let manager = BlockManager::new();
        manager
            .add_block(50, 50, BlockState::Pending)
            .await
            .unwrap();

        assert_eq!(manager.check_range(0..150).await, vec![0..150]);
    }

    #[tokio::test]
    async fn gaps_are_clipped_to_the_requested_range() {
        let manager = BlockManager::new();
        // 查询范围之前和之后各有一个已完成区块。
        manager.add_block(0, 20, BlockState::Complete).await.unwrap();
        manager
            .add_block(200, 50, BlockState::Complete)
            .await
            .unwrap();

        // 100..150 完全落在两个区块之间，整段缺失且不应越界。
        assert_eq!(manager.check_range(100..150).await, vec![100..150]);
        // 部分覆盖：0..20 已完成，缺口从 20 开始。
        assert_eq!(manager.check_range(10..60).await, vec![20..60]);
        // 完整覆盖在查询范围内的部分。
        assert_eq!(manager.check_range(0..20).await, Vec::<Range<u64>>::new());
        // 空/反向区间不产生输出。
        assert_eq!(manager.check_range(30..30).await, Vec::<Range<u64>>::new());
    }

    /// 结果必须有序且互不重叠——调用方会据此并发下载，重叠即重复写入。
    #[tokio::test]
    async fn reported_gaps_never_overlap() {
        let manager = BlockManager::new();
        manager.add_block(0, 10, BlockState::Complete).await.unwrap();
        manager.add_block(20, 10, BlockState::Pending).await.unwrap();
        manager
            .add_block(40, 10, BlockState::Complete)
            .await
            .unwrap();

        let gaps = manager.check_range(0..60).await;
        assert_eq!(gaps, vec![10..40, 50..60]);

        for pair in gaps.windows(2) {
            assert!(
                pair[0].end <= pair[1].start,
                "gaps overlap: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
    }
}
