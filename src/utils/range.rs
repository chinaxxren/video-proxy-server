use crate::utils::error::{ProxyError, Result};

/// 单个 HTTP 字节范围的两种形态。
///
/// RFC 7233 里 `bytes=-N` 表示「资源末尾 N 字节」，它的起点取决于总长度，
/// 没法用一对 `(start, end)` 表示。原先的解析器把这种请求直接判为非法
/// （`parts[0]` 为空 → "Invalid start position"），而它恰恰是播放器探测
/// 文件尾部 moov box 最常用的手段——MP4 的索引在尾部时，播放器开场就发
/// 一个 `bytes=-N` 找它。拒掉的后果是这类资源根本起播不了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// `bytes=N-M`，以及 `bytes=N-`（`end` 为 [`OPEN_ENDED`]）。
    Offset { start: u64, end: u64 },
    /// `bytes=-N`：末尾 N 字节。`length` 恒大于 0。
    Suffix { length: u64 },
}

impl RangeSpec {
    /// 转成内部统一使用的 `(start, requested_end)`。
    ///
    /// `Offset` 原样返回：`end` 可能仍是 [`OPEN_ENDED`]，交给下游的
    /// [`resolve_range`] 在知道总长度时再收敛，这条路径的行为不变。
    ///
    /// `Suffix` 必须有总长度才能定位起点。请求长度超过资源时按 RFC 7233
    /// 取整个资源，而不是报 416——`saturating_sub` 正好给出这个语义。
    pub fn endpoints(self, total_size: Option<u64>) -> Result<(u64, u64)> {
        match self {
            RangeSpec::Offset { start, end } => Ok((start, end)),
            RangeSpec::Suffix { length } => match total_size {
                Some(0) => Err(ProxyError::InvalidRange("资源长度为 0".to_string())),
                Some(total) => Ok((total.saturating_sub(length), total - 1)),
                None => Err(ProxyError::InvalidRange(
                    "总长度未知，无法定位后缀范围".to_string(),
                )),
            },
        }
    }
}

/// 解析单个字节范围，保留 `bytes=-N` 的后缀语义。
pub fn parse_range_spec(range: &str) -> Result<RangeSpec> {
    // 检查并移除前缀
    let Some(range) = range.strip_prefix("bytes=") else {
        return Err(ProxyError::Request("Invalid range format".to_string()));
    };
    if range.contains(',') {
        return Err(ProxyError::InvalidRange(
            "multiple byte ranges are not supported".to_string(),
        ));
    }

    // 分割范围
    let parts: Vec<&str> = range.split('-').collect();
    if parts.len() != 2 {
        return Err(ProxyError::Request("Invalid range format".to_string()));
    }
    let (first, second) = (parts[0].trim(), parts[1].trim());

    // 起点为空 = 后缀范围（`bytes=-N`）。必须在解析起点之前分流，否则
    // 空字符串会被当成非法数字。
    if first.is_empty() {
        let length = second
            .parse::<u64>()
            .map_err(|_| ProxyError::Request("Invalid suffix length".to_string()))?;
        if length == 0 {
            // 「末尾 0 字节」不可满足，RFC 7233 要求按 416 处理。
            return Err(ProxyError::InvalidRange("后缀范围长度不能为 0".to_string()));
        }
        return Ok(RangeSpec::Suffix { length });
    }

    // 解析开始位置
    let start = first
        .parse::<u64>()
        .map_err(|_| ProxyError::Request("Invalid start position".to_string()))?;

    // 解析结束位置
    let end = if second.is_empty() {
        OPEN_ENDED
    } else {
        second
            .parse::<u64>()
            .map_err(|_| ProxyError::Request("Invalid end position".to_string()))?
    };

    // 验证范围
    if start > end {
        return Err(ProxyError::Request(
            "Invalid range: start > end".to_string(),
        ));
    }

    Ok(RangeSpec::Offset { start, end })
}

/// 解析范围并要求它已经是普通的 `start-end` 形态。
///
/// 后缀范围必须先由 [`RangeSpec::endpoints`] 借总长度定位成具体区间；走到
/// 这里还是后缀形态，说明调用方漏了归一化，属于内部错误而不是客户端错误。
pub fn parse_range(range: &str) -> Result<(u64, u64)> {
    match parse_range_spec(range)? {
        RangeSpec::Offset { start, end } => Ok((start, end)),
        RangeSpec::Suffix { .. } => Err(ProxyError::InvalidRange(
            "后缀范围需先借总长度解析为具体区间".to_string(),
        )),
    }
}

/// 把内部区间写回 `Range` 头的形式。
///
/// 归一化的落点：后缀范围在这里已经变成具体区间，下游（尤其是
/// [`crate::data_source::net_source::NetSource`]）拿到的永远是
/// `bytes=start-end` 或 `bytes=start-`，不必再懂后缀语义。
pub fn format_range(start: u64, end: u64) -> String {
    if end == OPEN_ENDED {
        format!("bytes={}-", start)
    } else {
        format!("bytes={}-{}", start, end)
    }
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

/// 按上游实际声明的响应体长度收窄闭区间的右边界。
///
/// 上游可以合法地只返回请求区间的一个前缀：RFC 7233 并不要求 206 覆盖整个
/// 请求区间，CDN 普遍按固定块上限截断（请求 2MB 只给 64KB 是常见配置），
/// `verify_content_range` 也因此放行 `actual_end < expected_end`。
///
/// 不收窄的后果是响应头与响应体自相矛盾：`Content-Length` 按**请求**长度发出，
/// 而响应体只有上游给的那一段，客户端读到一半就撞上 EOF
/// （`end of file before message length reached`），播放器表现为卡死。
///
/// 收窄之后响应是一个诚实的、更短的 206。播放器从 `Content-Range` 就能看出
/// 只收到了前缀，会自己接着请求剩下的部分——这正是范围请求本来的工作方式。
///
/// `upstream_length` 为 0 表示上游没有给出长度，此时无从收窄，原样返回。
pub fn clamp_end_to_upstream_length(start: u64, end: u64, upstream_length: u64) -> u64 {
    match upstream_length {
        0 => end,
        // `length - 1` 不会下溢：0 已在上一分支被排除。
        // saturating_add 兜住 start + length 溢出 u64 的病态输入。
        length => end.min(start.saturating_add(length - 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn arbitrary_range_text_never_panics(value in ".{0,256}") {
            if let Ok(spec) = parse_range_spec(&value) {
                match spec {
                    RangeSpec::Offset { start, end } => prop_assert!(end == OPEN_ENDED || start <= end),
                    RangeSpec::Suffix { length } => prop_assert!(length > 0),
                }
            }
        }

        #[test]
        fn formatted_closed_ranges_round_trip(start in any::<u64>(), span in 0u64..=1_000_000) {
            let end = start.saturating_add(span);
            prop_assert_eq!(parse_range(&format_range(start, end)).unwrap(), (start, end));
        }
    }

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
            "bytes=10-9",
            "bytes=0-1,4-5",
            "bytes=0--1",
            "bytes=",
            "bytes=abc-10",
            "bytes=0-18446744073709551616",
            // 「末尾 0 字节」不可满足。
            "bytes=-0",
            "bytes=-abc",
        ] {
            assert!(parse_range_spec(value).is_err(), "accepted {value}");
        }
    }

    /// `bytes=-N` 是合法的后缀范围，播放器靠它找 MP4 尾部的 moov box。
    #[test]
    fn parses_suffix_ranges() {
        assert_eq!(
            parse_range_spec("bytes=-500").unwrap(),
            RangeSpec::Suffix { length: 500 }
        );
        assert_eq!(
            parse_range_spec("bytes=0-99").unwrap(),
            RangeSpec::Offset { start: 0, end: 99 }
        );
        assert_eq!(
            parse_range_spec("bytes=42-").unwrap(),
            RangeSpec::Offset {
                start: 42,
                end: OPEN_ENDED
            }
        );
    }

    #[test]
    fn suffix_range_endpoints_need_total_size() {
        let suffix = parse_range_spec("bytes=-500").unwrap();
        assert_eq!(suffix.endpoints(Some(2000)).unwrap(), (1500, 1999));
        // 请求长度超过资源时按 RFC 7233 取整个资源，而不是 416。
        assert_eq!(suffix.endpoints(Some(300)).unwrap(), (0, 299));
        assert!(suffix.endpoints(None).is_err());
        assert!(suffix.endpoints(Some(0)).is_err());
    }

    /// 后缀范围不能悄悄流进只认 `start-end` 的下游。
    #[test]
    fn parse_range_rejects_unnormalized_suffix() {
        assert!(parse_range("bytes=-500").is_err());
        assert_eq!(parse_range("bytes=100-199").unwrap(), (100, 199));
    }

    #[test]
    fn format_range_round_trips_through_parse() {
        for (start, end) in [(0u64, 99u64), (1500, 1999), (7, OPEN_ENDED)] {
            let text = format_range(start, end);
            assert_eq!(parse_range(&text).unwrap(), (start, end), "{text}");
        }
        assert_eq!(format_range(1500, 1999), "bytes=1500-1999");
        assert_eq!(format_range(42, OPEN_ENDED), "bytes=42-");
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
