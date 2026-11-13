//! ISO 8601 durations as Service Bus writes them (`PT1M`, `P14D`,
//! `P10675199DT2H48M5.4775807S`): weeks, days, hours, minutes and fractional
//! seconds. Years and months have no fixed length and are refused.

use std::time::Duration;

/// Parses `text`, or `None` if it is not a duration of that form.
pub(crate) fn parse(text: &str) -> Option<Duration> {
    let rest = text.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (rest, None),
    };
    let mut seconds = 0f64;
    let mut take = |part: &str, units: &[(char, f64)]| -> Option<()> {
        let mut number = String::new();
        let mut last_unit = None::<usize>;
        for ch in part.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                number.push(ch);
                continue;
            }
            let index = units.iter().position(|(unit, _)| *unit == ch)?;
            // Units appear at most once, largest first.
            if last_unit.is_some_and(|last| index <= last) || number.is_empty() {
                return None;
            }
            seconds += number.parse::<f64>().ok()? * units[index].1;
            number.clear();
            last_unit = Some(index);
        }
        number.is_empty().then_some(())
    };
    take(date, &[('W', 604_800.0), ('D', 86_400.0)])?;
    if let Some(time) = time {
        if time.is_empty() {
            return None;
        }
        take(time, &[('H', 3_600.0), ('M', 60.0), ('S', 1.0)])?;
    }
    (rest != "T" && !rest.is_empty()).then(|| Duration::from_secs_f64(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_bus_durations() {
        assert_eq!(parse("PT1M"), Some(Duration::from_secs(60)));
        assert_eq!(parse("PT30S"), Some(Duration::from_secs(30)));
        assert_eq!(parse("P14D"), Some(Duration::from_secs(14 * 86_400)));
        assert_eq!(parse("P1DT1H"), Some(Duration::from_secs(90_000)));
        assert_eq!(parse("PT0.5S"), Some(Duration::from_millis(500)));
        let forever = parse("P10675199DT2H48M5.4775807S").unwrap();
        assert!(forever.as_secs() > 900_000_000_000);
    }

    #[test]
    fn refuses_what_is_not_a_fixed_duration() {
        for text in ["", "P", "PT", "1M", "P1M", "P1Y", "PT1M1H", "PTM"] {
            assert_eq!(parse(text), None, "{text}");
        }
    }
}
