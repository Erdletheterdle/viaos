//! Compose scalar formats that are not plain numbers.

use crate::Error;

/// Parse a Compose duration such as `1m30s`, `10s`, `500ms`, or a bare number
/// of seconds, into whole seconds.
///
/// Via's manifest stores whole seconds because that is the resolution any of
/// these values are actually used at. A non-zero sub-second duration rounds up
/// to one second rather than truncating to zero, since a `0s` healthcheck
/// interval means something quite different from `500ms`.
pub fn parse_duration_secs(s: &str) -> Result<u64, Error> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::InvalidDuration {
            value: s.to_string(),
        });
    }
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }

    let mut total_nanos: u128 = 0;
    let mut chars = s.char_indices().peekable();
    let mut matched_any = false;

    while let Some(&(start, c)) = chars.peek() {
        if !c.is_ascii_digit() && c != '.' {
            return Err(Error::InvalidDuration {
                value: s.to_string(),
            });
        }
        let mut end = start;
        while let Some(&(i, c)) = chars.peek() {
            if c.is_ascii_digit() || c == '.' {
                end = i + c.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        let value: f64 = s[start..end].parse().map_err(|_| Error::InvalidDuration {
            value: s.to_string(),
        })?;

        let unit_start = end;
        let mut unit_end = end;
        while let Some(&(i, c)) = chars.peek() {
            if c.is_ascii_alphabetic() {
                unit_end = i + c.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        let unit = &s[unit_start..unit_end];
        let nanos_per = match unit {
            "ns" => 1.0,
            "us" | "µs" => 1_000.0,
            "ms" => 1_000_000.0,
            "s" => 1_000_000_000.0,
            "m" => 60.0 * 1_000_000_000.0,
            "h" => 3_600.0 * 1_000_000_000.0,
            _ => {
                return Err(Error::InvalidDuration {
                    value: s.to_string(),
                });
            }
        };
        total_nanos += (value * nanos_per) as u128;
        matched_any = true;
    }

    if !matched_any {
        return Err(Error::InvalidDuration {
            value: s.to_string(),
        });
    }
    let secs = total_nanos / 1_000_000_000;
    if secs == 0 && total_nanos > 0 {
        return Ok(1);
    }
    Ok(secs as u64)
}

/// Parse a Compose byte size such as `64m`, `1gb`, `512k`, or a bare number.
///
/// Compose's suffixes are powers of 1024, matching Docker rather than SI.
pub fn parse_size_bytes(s: &str) -> Result<u64, Error> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let split = s
        .find(|c: char| c.is_ascii_alphabetic())
        .ok_or_else(|| Error::InvalidSize {
            value: s.to_string(),
        })?;
    let (num, unit) = s.split_at(split);
    let value: f64 = num.trim().parse().map_err(|_| Error::InvalidSize {
        value: s.to_string(),
    })?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "b" => 1,
        "k" | "kb" => 1024,
        "m" | "mb" => 1024 * 1024,
        "g" | "gb" => 1024 * 1024 * 1024,
        "t" | "tb" => 1024u64 * 1024 * 1024 * 1024,
        _ => {
            return Err(Error::InvalidSize {
                value: s.to_string(),
            });
        }
    };
    Ok((value * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compound_durations() {
        assert_eq!(parse_duration_secs("1m30s").unwrap(), 90);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
        assert_eq!(parse_duration_secs("10s").unwrap(), 10);
        assert_eq!(parse_duration_secs("30").unwrap(), 30);
    }

    #[test]
    fn sub_second_rounds_up_rather_than_to_zero() {
        assert_eq!(parse_duration_secs("500ms").unwrap(), 1);
        assert_eq!(parse_duration_secs("0s").unwrap(), 0);
    }

    #[test]
    fn rejects_garbage_durations() {
        assert!(parse_duration_secs("soon").is_err());
        assert!(parse_duration_secs("10x").is_err());
        assert!(parse_duration_secs("").is_err());
    }

    #[test]
    fn sizes_use_1024_not_1000() {
        assert_eq!(parse_size_bytes("64m").unwrap(), 67_108_864);
        assert_eq!(parse_size_bytes("1gb").unwrap(), 1_073_741_824);
        assert_eq!(parse_size_bytes("2048").unwrap(), 2048);
    }
}
