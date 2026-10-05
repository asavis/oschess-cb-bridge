//! Database-relative selection keys, computed only while indexing.
use crate::store::Head;
use cbformat::game::Date;

/// A normalized calendar date, or zero for an unknown/invalid year or date.
pub fn date(d: Date) -> u32 {
    let (y, m, day) = (u32::from(d.year()), u32::from(d.month()).max(1), u32::from(d.day()).max(1));
    if y == 0 || m > 12 || day > days(y, m) {
        return 0;
    }
    y * 10000 + m * 100 + day
}
fn days(y: u32, m: u32) -> u32 {
    match m {
        2 => {
            if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}
fn years_before(date: u32, years: u32) -> u32 {
    let y = (date / 10000).saturating_sub(years);
    let m = date / 100 % 100;
    y * 10000 + m * 100 + (date % 100).min(days(y, m))
}
/// Twice the average: no rounding, and each unknown rating contributes 1500.
/// The supported formats hold ratings in at most 16 bits.
pub fn rating_sum(r: &impl Head) -> u32 {
    let rating = |v: i32| if v <= 0 { 1500 } else { (v as u32).min(u16::MAX.into()) };
    let (w, b) = r.elo();
    rating(w) + rating(b)
}
/// Higher is better. Date selects a tier only; it is absent from the key.
pub fn key(sum: u32, date: u32, anchor: u32) -> u32 {
    let tier = if date == 0 || anchor == 0 {
        0
    } else if sum >= 5400 && date >= years_before(anchor, 1) {
        3
    } else if sum >= 5200 && date >= years_before(anchor, 3) {
        2
    } else if sum >= 4800 && date >= years_before(anchor, 5) {
        1
    } else {
        0
    };
    (tier << 17) | sum.min(2 * u32::from(u16::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusive_calendar_boundaries_and_exact_half_point_thresholds() {
        let anchor = 20240229;
        for (sum, date, tier) in [
            (5400, 20230228, 3),
            (5399, 20230228, 2),
            (5401, 20230228, 3),
            (5400, 20230227, 2),
            (5200, 20210228, 2),
            (5199, 20210228, 1),
            (5200, 20210227, 1),
            (4800, 20190228, 1),
            (4799, 20190228, 0),
            (4800, 20190227, 0),
            (65535, 0, 0),
        ] {
            assert_eq!(key(sum, date, anchor) >> 17, tier, "{sum} {date}");
        }
        assert_eq!(key(5400, 20230228, anchor), key(5400, anchor, anchor));
        assert!(key(5401, 20230228, anchor) > key(5400, anchor, anchor));
        assert!(key(5400, anchor, anchor) > key(65535, 19000101, anchor));
        assert_eq!(key(5400, anchor, 0), 5400);
        assert_eq!(years_before(20000229, 100), 19000228);
        assert_eq!(years_before(20040229, 4), 20000229);
    }

    #[test]
    fn dates_default_missing_parts_and_refuse_invalid_calendar_dates() {
        let d = |y, m, day| date(Date(y << 9 | m << 5 | day));
        assert_eq!(d(1858, 0, 0), 18580101);
        assert_eq!(d(1858, 12, 0), 18581201);
        assert_eq!(d(1858, 0, 12), 18580112);
        assert_eq!(d(0, 12, 12), 0);
        assert_eq!(d(2023, 2, 29), 0);
        assert_eq!(d(2024, 2, 29), 20240229);
        assert_eq!(d(2024, 13, 1), 0);
        assert_eq!(d(2024, 4, 31), 0);
    }

    #[test]
    fn missing_ratings_are_substituted_individually_without_changing_the_source() {
        for (white, black, want) in [(0i16, 0i16, 3000), (2701, 0, 4201), (0, 2701, 4201), (2700, 2701, 5401)] {
            let mut bytes = [0u8; 192];
            bytes[0x60..0x62].copy_from_slice(&white.to_le_bytes());
            bytes[0x70..0x72].copy_from_slice(&black.to_le_bytes());
            let row = cbformat::v2::Record::from_bytes(1, &bytes);
            assert_eq!(rating_sum(&row), want);
            assert_eq!(row.elo(), (i32::from(white), i32::from(black)));
        }
    }
}
