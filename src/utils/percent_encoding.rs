use std::borrow::Cow;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PercentDecodeError {
    InvalidEscape(usize),
    InvalidUtf8,
}

impl fmt::Display for PercentDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEscape(index) => {
                write!(formatter, "invalid percent escape at byte {index}")
            }
            Self::InvalidUtf8 => formatter.write_str("decoded value is not valid UTF-8"),
        }
    }
}

pub fn encode_component(value: &str) -> Cow<'_, str> {
    if value.bytes().all(is_unreserved) {
        return Cow::Borrowed(value);
    }

    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if is_unreserved(byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    Cow::Owned(encoded)
}

pub fn decode_component(value: &str) -> Result<Cow<'_, str>, PercentDecodeError> {
    if !value.as_bytes().contains(&b'%') {
        return Ok(Cow::Borrowed(value));
    }

    let input = value.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'%' {
            decoded.push(input[index]);
            index += 1;
            continue;
        }

        let high = input
            .get(index + 1)
            .and_then(|byte| hex_value(*byte))
            .ok_or(PercentDecodeError::InvalidEscape(index))?;
        let low = input
            .get(index + 2)
            .and_then(|byte| hex_value(*byte))
            .ok_or(PercentDecodeError::InvalidEscape(index))?;
        decoded.push((high << 4) | low);
        index += 3;
    }

    String::from_utf8(decoded)
        .map(Cow::Owned)
        .map_err(|_| PercentDecodeError::InvalidUtf8)
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_round_trip_handles_urls_and_unicode() {
        let original = "https://media.example/视频.ts?token=a+b&x=1";
        let encoded = encode_component(original);
        assert!(encoded.contains("%E8%A7%86%E9%A2%91"));
        assert_eq!(decode_component(&encoded).unwrap(), original);
    }

    #[test]
    fn unreserved_input_stays_borrowed() {
        assert!(matches!(encode_component("a-Z_0.~"), Cow::Borrowed(_)));
        assert!(matches!(decode_component("a-Z_0.~"), Ok(Cow::Borrowed(_))));
    }

    #[test]
    fn malformed_escapes_are_rejected() {
        for value in ["%", "%2", "%GG", "before%4Zafter"] {
            assert!(matches!(
                decode_component(value),
                Err(PercentDecodeError::InvalidEscape(_))
            ));
        }
    }

    #[test]
    fn decoded_bytes_must_be_utf8() {
        assert_eq!(
            decode_component("%FF"),
            Err(PercentDecodeError::InvalidUtf8)
        );
    }
}
