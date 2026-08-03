use crate::utils::error::{ProxyError, Result};

pub fn parse_range(range: &str) -> Result<(u64, u64)> {
    // 检查前缀
    if !range.starts_with("bytes=") {
        return Err(ProxyError::Request("Invalid range format".to_string()));
    }

    // 移除前缀
    let range = &range[6..];

    // 分割范围
    let parts: Vec<&str> = range.split('-').collect();
    if parts.len() != 2 {
        return Err(ProxyError::Request("Invalid range format".to_string()));
    }

    // 解析开始位置
    let start = parts[0]
        .parse::<u64>()
        .map_err(|_| ProxyError::Request("Invalid start position".to_string()))?;

    // 解析结束位置
    let end = if parts[1].is_empty() {
        u64::MAX
    } else {
        parts[1]
            .parse::<u64>()
            .map_err(|_| ProxyError::Request("Invalid end position".to_string()))?
    };

    // 验证范围
    if start > end {
        return Err(ProxyError::Request(
            "Invalid range: start > end".to_string(),
        ));
    }

    Ok((start, end))
}

/// `bytes=N-` 这类开区间请求在 [`parse_range`] 中的结束哨兵值。
///
/// 这个值**不能**直接参与算术运算，必须先经 [`resolve_range`] 收敛成真实的
/// 闭区间，否则 `end - start + 1` 之类的计算会溢出。
pub const OPEN_ENDED: u64 = u64::MAX;

/// 把 [`parse_range`] 产出的范围收敛成闭区间。
///
/// `total_size` 为 `None` 表示总长度未知；此时开区间无法确定终点，直接报错，
/// 而不是把 [`OPEN_ENDED`] 泄漏给下游算术。
pub fn resolve_range(start: u64, end: u64, total_size: Option<u64>) -> Result<(u64, u64)> {
    match total_size {
        Some(0) => Err(ProxyError::InvalidRange("资源长度为 0".to_string())),
        Some(total) => {
            let last = total - 1;
            if start > last {
                return Err(ProxyError::InvalidRange(format!(
                    "范围起点 {} 超出资源长度 {}",
                    start, total
                )));
            }
            Ok((start, end.min(last)))
        }
        None if end == OPEN_ENDED => Err(ProxyError::InvalidRange(
            "总长度未知，无法确定开区间的结束位置".to_string(),
        )),
        None => Ok((start, end)),
    }
}

/// 计算闭区间 `start..=end` 的字节数，溢出时报错而非 panic。
pub fn range_length(start: u64, end: u64) -> Result<u64> {
    end.checked_sub(start)
        .and_then(|span| span.checked_add(1))
        .ok_or_else(|| ProxyError::InvalidRange(format!("无效范围: {}-{}", start, end)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_closed_and_open_ended_ranges() {
        assert_eq!(parse_range("bytes=0-65535").unwrap(), (0, 65535));
        assert_eq!(parse_range("bytes=42-").unwrap(), (42, u64::MAX));
        assert_eq!(parse_range("bytes=7-7").unwrap(), (7, 7));
    }

    #[test]
    fn rejects_unsupported_or_malformed_ranges() {
        for value in [
            "items=0-1",
            "bytes=-10",
            "bytes=10-9",
            "bytes=0-1,4-5",
            "bytes=0--1",
            "bytes=",
            "bytes=abc-10",
            "bytes=0-18446744073709551616",
        ] {
            assert!(parse_range(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn resolve_range_clamps_open_ended_to_total_size() {
        assert_eq!(resolve_range(0, OPEN_ENDED, Some(100)).unwrap(), (0, 99));
        assert_eq!(resolve_range(10, OPEN_ENDED, Some(100)).unwrap(), (10, 99));
        assert_eq!(resolve_range(0, 4096, Some(100)).unwrap(), (0, 99));
        assert_eq!(resolve_range(0, 49, Some(100)).unwrap(), (0, 49));
    }

    #[test]
    fn resolve_range_rejects_unsatisfiable_and_unknown_length() {
        assert!(resolve_range(100, OPEN_ENDED, Some(100)).is_err());
        assert!(resolve_range(0, OPEN_ENDED, Some(0)).is_err());
        assert!(resolve_range(0, OPEN_ENDED, None).is_err());
        assert_eq!(resolve_range(0, 99, None).unwrap(), (0, 99));
    }

    #[test]
    fn range_length_never_overflows() {
        assert_eq!(range_length(0, 0).unwrap(), 1);
        assert_eq!(range_length(10, 19).unwrap(), 10);
        // 回归：`end - start + 1` 在 OPEN_ENDED 上会溢出 panic。
        assert!(range_length(0, OPEN_ENDED).is_err());
        assert!(range_length(5, 4).is_err());
    }
}
