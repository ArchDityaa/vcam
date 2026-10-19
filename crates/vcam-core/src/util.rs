use std::path::PathBuf;

/// Expand a leading `~/` to `$HOME`.
pub fn expand_path(input: &str) -> PathBuf {
    if input == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(input)
}

/// Format milliseconds as `HH:MM:SS`.
pub fn format_time_ms(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// Parse `SS`, `MM:SS`, or `HH:MM:SS`. Fractional seconds are accepted.
pub fn parse_time_ms(input: &str) -> Result<u64, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("time value is empty".into());
    }
    let parts: Vec<&str> = input.split(':').collect();
    if parts.len() > 3 {
        return Err(format!("invalid time '{input}'"));
    }
    let mut seconds = 0f64;
    for (index, part) in parts.iter().enumerate() {
        let value: f64 = part
            .parse()
            .map_err(|_| format!("invalid time component '{part}'"))?;
        if value < 0.0 {
            return Err(format!("invalid negative time '{input}'"));
        }
        let multiplier = 60f64.powi((parts.len() - 1 - index) as i32);
        seconds += value * multiplier;
    }
    if !seconds.is_finite() {
        return Err(format!("invalid time '{input}'"));
    }
    Ok((seconds * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clock_values() {
        assert_eq!(parse_time_ms("12").unwrap(), 12_000);
        assert_eq!(parse_time_ms("1:30").unwrap(), 90_000);
        assert_eq!(parse_time_ms("01:02:03").unwrap(), 3_723_000);
        assert_eq!(parse_time_ms("1.5").unwrap(), 1_500);
        assert!(parse_time_ms("1:2:3:4").is_err());
        assert!(parse_time_ms("abc").is_err());
    }

    #[test]
    fn formats_clock_values() {
        assert_eq!(format_time_ms(0), "00:00");
        assert_eq!(format_time_ms(90_000), "01:30");
        assert_eq!(format_time_ms(3_723_000), "01:02:03");
    }
}
