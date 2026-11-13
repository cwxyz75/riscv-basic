//! Calendar time (C11 7.27) in UTC: `gmtime`, `timegm` (the normalising core of `mktime`) and
//! `strftime` in the "C" locale. There is no time zone database: local time is UTC.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tm {
    pub sec: i32,
    pub min: i32,
    pub hour: i32,
    pub mday: i32,
    /// 0-11
    pub mon: i32,
    /// Years since 1900.
    pub year: i32,
    /// 0 is Sunday.
    pub wday: i32,
    /// 0-365.
    pub yday: i32,
}

/// Days from 1970-01-01 to the civil date (proleptic Gregorian), Howard Hinnant's algorithm.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Break `t` (seconds since the epoch) into UTC fields. `None` if the year does not fit.
pub fn gmtime(t: i64) -> Option<Tm> {
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let year = i32::try_from(y - 1900).ok()?;
    let yday = days - days_from_civil(y, 1, 1);
    Some(Tm {
        sec: (rem % 60) as i32,
        min: (rem / 60 % 60) as i32,
        hour: (rem / 3600) as i32,
        mday: d as i32,
        mon: (m - 1) as i32,
        year,
        wday: (days + 4).rem_euclid(7) as i32,
        yday: yday as i32,
    })
}

/// Seconds since the epoch for fields that need not be normalised (`mktime` semantics: months
/// outside 0-11 carry into the year, the other fields are added as offsets).
pub fn timegm(tm: &Tm) -> Option<i64> {
    let mon = i64::from(tm.mon);
    let y = i64::from(tm.year) + 1900 + mon.div_euclid(12);
    let m = mon.rem_euclid(12) + 1;
    let days = days_from_civil(y, m, 1) + i64::from(tm.mday) - 1;
    days.checked_mul(86_400)?
        .checked_add(i64::from(tm.hour) * 3600 + i64::from(tm.min) * 60 + i64::from(tm.sec))
}

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// ISO 8601 week number and week-based year.
fn iso_week(tm: &Tm) -> (i64, i64) {
    let year = i64::from(tm.year) + 1900;
    let wday = (i64::from(tm.wday) + 6) % 7; // Monday = 0
    let mut week = (i64::from(tm.yday) - wday + 10) / 7;
    let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let weeks_in = |y: i64| {
        // a year has 53 weeks if Jan 1 is a Thursday, or a Wednesday in a leap year
        let jan1 = (days_from_civil(y, 1, 1) + 4).rem_euclid(7); // 0 = Sunday
        if jan1 == 4 || (leap(y) && jan1 == 3) {
            53
        } else {
            52
        }
    };
    if week < 1 {
        return (weeks_in(year - 1), year - 1);
    }
    if week > weeks_in(year) {
        week = 1;
        return (week, year + 1);
    }
    (week, year)
}

/// A conversion `strftime` does not know (or a format that ends inside one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadConversion;

/// `strftime` in the "C" locale. `Err` for an unknown conversion.
pub fn strftime(fmt: &[u8], tm: &Tm, out: &mut Vec<u8>) -> Result<(), BadConversion> {
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        i += 1;
        // C99 E and O modifiers select alternative forms that the C locale does not have
        if matches!(fmt.get(i), Some(b'E') | Some(b'O')) {
            i += 1;
        }
        let Some(&c) = fmt.get(i) else {
            return Err(BadConversion);
        };
        i += 1;
        let wd = DAYS[tm.wday.rem_euclid(7) as usize];
        let mn = MONTHS[tm.mon.rem_euclid(12) as usize];
        let year = i64::from(tm.year) + 1900;
        let s: String = match c {
            b'a' => wd[..3].into(),
            b'A' => wd.into(),
            b'b' | b'h' => mn[..3].into(),
            b'B' => mn.into(),
            b'c' => {
                let mut v = Vec::new();
                strftime(b"%a %b %e %H:%M:%S %Y", tm, &mut v)?;
                String::from_utf8_lossy(&v).into_owned()
            }
            b'C' => format!("{:02}", year.div_euclid(100)),
            b'd' => format!("{:02}", tm.mday),
            b'D' | b'x' => {
                let mut v = Vec::new();
                strftime(b"%m/%d/%y", tm, &mut v)?;
                String::from_utf8_lossy(&v).into_owned()
            }
            b'e' => format!("{:2}", tm.mday),
            b'F' => format!("{year}-{:02}-{:02}", tm.mon + 1, tm.mday),
            b'g' => format!("{:02}", iso_week(tm).1.rem_euclid(100)),
            b'G' => format!("{}", iso_week(tm).1),
            b'H' => format!("{:02}", tm.hour),
            b'I' => format!("{:02}", if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 }),
            b'j' => format!("{:03}", tm.yday + 1),
            b'm' => format!("{:02}", tm.mon + 1),
            b'M' => format!("{:02}", tm.min),
            b'n' => "\n".into(),
            b'p' => if tm.hour >= 12 { "PM" } else { "AM" }.into(),
            b'r' => format!(
                "{:02}:{:02}:{:02} {}",
                if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 },
                tm.min,
                tm.sec,
                if tm.hour >= 12 { "PM" } else { "AM" }
            ),
            b'R' => format!("{:02}:{:02}", tm.hour, tm.min),
            b'S' => format!("{:02}", tm.sec),
            b't' => "\t".into(),
            b'T' | b'X' => format!("{:02}:{:02}:{:02}", tm.hour, tm.min, tm.sec),
            b'u' => format!("{}", if tm.wday == 0 { 7 } else { tm.wday }),
            b'U' => format!("{:02}", (tm.yday + 7 - tm.wday) / 7),
            b'V' => format!("{:02}", iso_week(tm).0),
            b'w' => format!("{}", tm.wday),
            b'W' => format!("{:02}", (tm.yday + 7 - (tm.wday + 6) % 7) / 7),
            b'y' => format!("{:02}", year.rem_euclid(100)),
            b'Y' => format!("{year}"),
            b'z' => "+0000".into(),
            b'Z' => "UTC".into(),
            b'%' => "%".into(),
            _ => return Err(BadConversion),
        };
        out.extend_from_slice(s.as_bytes());
    }
    Ok(())
}
