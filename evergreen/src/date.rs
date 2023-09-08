use crate::result::EgResult;
use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, TimeZone};
use chrono_tz::Tz;
use regex::{Regex, Captures};

const INTERVAL_PART_REGEX: &str = r#"\s*([\+-]?)\s*(\d+)\s*(\w+)\s*"#;
const INTERVAL_HMS_REGEX: &str = r#"(\d{2,}):(\d{2}):(\d{2})"#;

/// Turn an interval string into a number of seconds.
///
/// Supports a subset of the language, which is typically enough
/// for our use cases.  For better parsing, if needed, we could use
/// (e.g.) https://crates.io/crates/parse_duration
///
/// ```
/// use evergreen::date;
///
/// let seconds = date::interval_to_seconds("02:20:05").expect("Parse OK");
/// assert_eq!(seconds, 8405);
///
/// let seconds = date::interval_to_seconds("1 min 2 seconds").expect("Parse OK");
/// assert_eq!(seconds, 62);
/// ```
pub fn interval_to_seconds(interval: &str) -> Result<i64, String> {
    let hms_reg = Regex::new(INTERVAL_HMS_REGEX).unwrap();
    let part_reg = Regex::new(INTERVAL_PART_REGEX).unwrap();

    let mut interval = interval.to_lowercase();
    interval = interval.replace("and", ",");
    interval = interval.replace(",", " ");

    // Format hh:mm:ss
    let interval = hms_reg.replace(&interval, |caps: &Captures| {
        // caps[0] is the full source string
        format!("{} h {} min {} s", &caps[1], &caps[2], &caps[3])
    });

    let mut amount = 0;
    for (_, [sign, count, itype]) in part_reg.captures_iter(&interval).map(|c| c.extract()) {
        let count = match count.parse::<i64>() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Invalid interval number: {count} {e} from {interval}");
                continue;
            }
        };

        let change = if itype.starts_with("s") {
            count
        } else if itype.starts_with("min") {
            count * 60
        } else if itype.starts_with("h") {
            count * 60 * 60
        } else if itype.starts_with("d") {
            count * 60 * 60 * 24
        } else if itype.starts_with("w") {
            count * 60 * 60 * 24 * 7
        } else if itype.starts_with("mon") {
            (count * 60 * 60 * 24 * 365) / 12
        } else if itype.starts_with("y") {
            count * 60 * 60 * 24 * 365
        } else {
            0
        };

        if sign == "-" {
            amount -= change;
        } else {
            amount += change;
        }
    }

    Ok(amount)
}

/// Current date/time with a fixed offset matching the local time zone.
pub fn now_local() -> DateTime<FixedOffset> {
    now()
}

/// Current date/time with a fixed offset matching the local time zone.
pub fn now() -> DateTime<FixedOffset> {
    to_local_timezone_fixed(Local::now().into())
}

/// Parse an ISO date string and return a date which retains its original
/// time zone.
///
/// If the datetime string is in the Local timezone, for example, the
/// DateTime value produced will also be in the local timezone.
///
/// ```
/// use evergreen::date;
/// use chrono::{DateTime, FixedOffset, Local};
///
/// let dt = date::parse_datetime("2023-07-11T12:00:00-0200");
/// assert!(dt.is_ok());
///
/// let dt2 = date::parse_datetime("2023-07-11T11:00:00-0300");
/// assert!(dt2.is_ok());
///
/// assert_eq!(dt.unwrap(), dt2.unwrap());
///
/// let dt = date::parse_datetime("2023-07-11");
/// assert!(dt.is_ok());
///
/// let dt = date::parse_datetime("2023-07-11 HOWDY");
/// assert!(dt.is_err());
///
/// ```
pub fn parse_datetime(dt: &str) -> EgResult<DateTime<FixedOffset>> {
    if dt.len() > 10 {
        // Assume its a full date + time
        return match dt.parse::<DateTime<FixedOffset>>() {
            Ok(d) => Ok(d),
            Err(e) => return Err(format!("Could not parse datetime string: {e} {dt}").into()),
        };
    }

    if dt.len() < 10 {
        return Err(format!("Invalid date string: {dt}").into());
    }

    // Assumes it's just a YYYY-MM-DD
    let date = match dt.parse::<NaiveDate>() {
        Ok(d) => d,
        Err(e) => return Err(format!("Could not parse date string: {e} {dt}").into()),
    };

    // If we only have a date, use the local timezone.
    let local_date = match Local
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .earliest()
    {
        Some(d) => d,
        None => return Err(format!("Could not parse date string: {dt}").into()),
    };

    Ok(local_date.into())
}

/// Turn a DateTime into the kind of date string we like in these parts.
/// ```
/// use evergreen::date;
/// use chrono::{DateTime, FixedOffset, Local};
/// let dt: DateTime<FixedOffset> = "2023-07-11T12:00:00-0700".parse().unwrap();
/// assert_eq!(date::to_iso(&dt), "2023-07-11T12:00:00-0700");
/// ```
pub fn to_iso(dt: &DateTime<FixedOffset>) -> String {
    dt.format("%FT%T%z").to_string()
}

/// Same as to_iso but includes milliseconds
/// e.g. 2023-09-08T10:59:01.687-0400
pub fn to_iso_millis(dt: &DateTime<FixedOffset>) -> String {
    dt.format("%FT%T%.3f%z").to_string()
}

/// Translate a DateTime into the Local timezone while leaving the
/// DateTime as a FixedOffset DateTime.
/// ```
/// use evergreen::date;
/// use chrono::{DateTime, FixedOffset, Local};
/// let dt: DateTime<FixedOffset> = "2023-07-11T12:00:00-0200".parse().unwrap();
/// let dt2: DateTime<FixedOffset> = date::to_local_timezone_fixed(dt);
///
///
/// assert_eq!(dt2.offset(), Local::now().offset());
///
/// // String output will vary by locale, but the dates will be equivalent.
/// assert_eq!(dt, dt2);
/// ```
pub fn to_local_timezone_fixed(dt: DateTime<FixedOffset>) -> DateTime<FixedOffset> {
    let local: DateTime<Local> = dt.into();

    // Translate back to a fixed time zone using our newly
    // acquired local time zone as the offset.
    local.with_timezone(local.offset())
}

/// Apply a timezone to a DateTime value.
///
/// This does not change the date/time, only the lense through which
/// the datetime is interpreted (string representation, hour, day of week, etc.).
///
/// To apply a timezone to a Local or Utc value, just:
/// set_timezone(local_date.into(), "America/New_York");
///
/// ```
/// use evergreen::date;
/// use chrono::{DateTime, FixedOffset};
/// let dt: DateTime<FixedOffset> = "2023-07-11T12:00:00-0400".parse().unwrap();
/// let dt = date::set_timezone(dt, "GMT").unwrap();
/// assert_eq!(date::to_iso(&dt), "2023-07-11T16:00:00+0000");
/// ```
pub fn set_timezone(
    dt: DateTime<FixedOffset>,
    timezone: &str,
) -> Result<DateTime<FixedOffset>, String> {
    if timezone == "local" {
        return Ok(to_local_timezone_fixed(dt));
    }

    // Parse the time zone string.
    let tz: Tz = timezone
        .parse()
        .or_else(|e| Err(format!("Cannot parse timezone: {timezone} {e}")))?;

    let modified = dt.with_timezone(&tz);

    let fixed: DateTime<FixedOffset> = match modified.format("%FT%T%z").to_string().parse() {
        Ok(f) => f,
        Err(e) => Err(format!("Cannot reconstruct date: {modified:?} : {e}"))?,
    };

    Ok(fixed)
}

/// Set the hour/minute/seconds on a DateTime, retaining the original date and timezone.
///
/// (There's gotta be a better way...)
///
/// ```
/// use evergreen::date;
/// use chrono::{DateTime, FixedOffset};
/// let dt: DateTime<FixedOffset> = "2023-07-11T01:25:18-0400".parse().unwrap();
/// let dt = date::set_hms(&dt, 23, 59, 59).unwrap();
/// assert_eq!(date::to_iso(&dt), "2023-07-11T23:59:59-0400");
/// ```
pub fn set_hms(
    date: &DateTime<FixedOffset>,
    hours: u32,
    minutes: u32,
    seconds: u32,
) -> Result<DateTime<FixedOffset>, String> {
    let offset = FixedOffset::from_offset(date.offset());

    let datetime = match date.date_naive().and_hms_opt(hours, minutes, seconds) {
        Some(dt) => dt,
        None => Err(format!("Could not set time to {hours}:{minutes}:{seconds}"))?,
    };

    // and_local_timezone() can return multiples in cases where it's ambiguous.
    let new_date: DateTime<FixedOffset> = match datetime.and_local_timezone(offset).single() {
        Some(d) => d,
        None => Err(format!("Error setting timezone for datetime {datetime:?}"))?,
    };

    Ok(new_date)
}
