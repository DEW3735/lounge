use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use lounge_plugin_sdk::{
    self as sdk, HttpRequest, Item, Kind, Manifest, RefreshContext, RowSpec, RowStyle,
    SettingKind, SettingSpec, host,
};
use rrule::{RRuleSet, Tz};
use std::collections::{HashMap, HashSet};

struct GoogleCalendar;

const ROW: &str = "upcoming";
const SECRET: &str = "ical_address";
const GOOGLE_FEED: &str = "https://calendar.google.com/calendar/ical/";
const MAX_ITEMS: usize = 20;

impl sdk::Plugin for GoogleCalendar {
    fn manifest() -> Manifest {
        Manifest {
            id: "tv.lounge.google-calendar".into(),
            name: "Google Calendar".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Upcoming events from one Google Calendar.".into(),
            publisher_name: "DEW Lounge".into(),
            kinds: vec![Kind::Content],
            fetch_hosts: vec!["calendar.google.com".into()],
            refresh_minutes: 30,
            rows: vec![RowSpec {
                id: ROW.into(),
                title: "Upcoming events".into(),
                style: RowStyle::Compact,
            }],
            settings: vec![SettingSpec {
                key: SECRET.into(),
                label: "Secret iCal address".into(),
                kind: SettingKind::Password,
                required: true,
                help: Some("In Google Calendar on a computer, open Settings, choose a calendar, and copy its secret iCal address under Integrate calendar.".into()),
                default: None,
            }],
            homepage: Some("https://calendar.google.com".into()),
            self_signed_servers: false,
            for_apps: Vec::new(),
            sent_to: Default::default(),
        }
    }

    fn refresh(ctx: &RefreshContext) -> Result<Vec<Item>, String> {
        let address = host::secret(SECRET)
            .filter(|address| valid_address(address.trim()))
            .ok_or("Enter the Google Calendar secret iCal address in plugin settings.")?;
        let response = host::fetch(&HttpRequest::get(address.trim())).map_err(|_| "Could not reach Google Calendar.")?;
        if response.status != 200 {
            return Err(format!("Google Calendar returned HTTP {}. Check the iCal address.", response.status));
        }
        upcoming(&response.body, ctx.now_ms, ctx.utc_offset_minutes)
    }
}

sdk::export_plugin!(GoogleCalendar);

fn valid_address(address: &str) -> bool {
    let Some(path) = address.strip_prefix(GOOGLE_FEED) else { return false };
    let parts: Vec<_> = path.split('/').collect();
    matches!(parts.as_slice(), [calendar, secret, "basic.ics"] if !calendar.is_empty()
        && secret.starts_with("private-") && secret.len() > "private-".len())
        && !path.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '?' | '#' | '\\'))
}

#[derive(Default)]
struct Event {
    uid: String,
    title: String,
    start: Option<Stamp>,
    end: Option<Stamp>,
    original_start: Option<Stamp>,
    cancelled: bool,
    rules: Vec<String>,
}

struct Stamp {
    value: String,
    zone: Option<String>,
}

impl Stamp {
    fn read(key: &str, value: &str) -> Self {
        let zone = key.split(';').skip(1)
            .find_map(|param| param.strip_prefix("TZID="))
            .map(|zone| zone.trim_matches('"').to_owned());
        Self { value: value.into(), zone }
    }

    fn all_day(&self) -> bool {
        self.value.len() == 8
    }

    fn date(&self, zone: Tz) -> Option<DateTime<Tz>> {
        if self.all_day() {
            return Some(NaiveDate::parse_from_str(&self.value, "%Y%m%d").ok()?
                .and_hms_opt(0, 0, 0)?.and_local_timezone(Tz::UTC).single()?);
        }
        let date = NaiveDateTime::parse_from_str(self.value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()?;
        let zone = if self.value.ends_with('Z') {
            Tz::UTC
        } else {
            self.zone.as_deref().map(str::parse).transpose().ok()?.unwrap_or(zone)
        };
        date.and_local_timezone(zone).earliest()
    }
}

fn unfold(feed: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in feed.lines() {
        match (line.starts_with([' ', '\t']), lines.last_mut()) {
            (true, Some(previous)) => previous.push_str(&line[1..]),
            _ => lines.push(line.into()),
        }
    }
    lines
}

fn read_feed(feed: &str) -> Result<(Tz, Vec<Event>), String> {
    let lines = unfold(feed);
    if lines.first().is_none_or(|line| line.trim_start_matches('\u{feff}') != "BEGIN:VCALENDAR")
        || lines.iter().rfind(|line| !line.is_empty()).is_none_or(|line| line != "END:VCALENDAR")
    {
        return Err("Google Calendar did not return a complete iCal feed.".into());
    }
    let mut zone = Tz::UTC;
    let mut events = Vec::new();
    let mut event = Event::default();
    let mut depth = 0_u32;
    let mut event_depth = None;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else { continue };
        match key.split(';').next().unwrap_or_default() {
            "BEGIN" => {
                depth += 1;
                if value == "VEVENT" && depth == 2 {
                    event = Event::default();
                    event_depth = Some(depth);
                }
            }
            "END" => {
                if event_depth == Some(depth) && value == "VEVENT" {
                    if !event.uid.is_empty() {
                        events.push(std::mem::take(&mut event));
                    }
                    event_depth = None;
                }
                depth = depth.saturating_sub(1);
            }
            "X-WR-TIMEZONE" if depth == 1 => zone = value.parse().unwrap_or(Tz::UTC),
            name if event_depth == Some(depth) => match name {
                "UID" => event.uid = value.into(),
                "SUMMARY" => event.title = unescape(value),
                "DTSTART" => event.start = Some(Stamp::read(key, value)),
                "DTEND" => event.end = Some(Stamp::read(key, value)),
                "RECURRENCE-ID" => event.original_start = Some(Stamp::read(key, value)),
                "STATUS" => event.cancelled = value == "CANCELLED",
                "RRULE" | "RDATE" | "EXDATE" => event.rules.push(line),
                _ => {}
            },
            _ => {}
        }
    }
    Ok((zone, events))
}

fn unescape(value: &str) -> String {
    let mut chars = value.chars();
    let mut text = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some('n' | 'N') => text.push(' '),
                Some(escaped) => text.push(escaped),
                None => text.push('\\'),
            },
            _ => text.push(ch),
        }
    }
    text
}

fn occurrences(event: &Event, start: DateTime<Tz>, after: DateTime<Tz>, before: DateTime<Tz>) -> Vec<DateTime<Tz>> {
    if event.rules.is_empty() || event.original_start.is_some() {
        return vec![start];
    }
    let start_line = if start.timezone() == Tz::UTC {
        format!("DTSTART:{}", start.format("%Y%m%dT%H%M%SZ"))
    } else {
        format!("DTSTART;TZID={}:{}", start.timezone(), start.format("%Y%m%dT%H%M%S"))
    };
    let mut lines = vec![start_line];
    for rule in &event.rules {
        // Recurrence libraries represent all-day dates as midnight. The calendar date is preserved.
        lines.push(rule.replace(";VALUE=DATE:", ":").split_once(':').map_or(rule.clone(), |(key, values)| {
            let values = values.split(',').map(|value| {
                if value.len() == 8 && value.bytes().all(|ch| ch.is_ascii_digit()) {
                    format!("{value}T000000Z")
                } else {
                    value.to_owned()
                }
            }).collect::<Vec<_>>();
            format!("{key}:{}", values.join(","))
        }));
    }
    lines.join("\n").parse::<RRuleSet>().map(|set| set.after(after).before(before).all(64).dates)
        .unwrap_or_else(|_| vec![start])
}

fn upcoming(feed: &str, now_ms: i64, offset_minutes: i32) -> Result<Vec<Item>, String> {
    let (zone, events) = read_feed(feed)?;
    let offset = FixedOffset::east_opt(offset_minutes.saturating_mul(60)).unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
    let now = DateTime::<Utc>::from_timestamp_millis(now_ms).ok_or("The device clock is out of range.")?;
    let local_today = now.with_timezone(&offset).date_naive();
    let today = local_today.and_hms_opt(0, 0, 0).ok_or("The device date is out of range.")?
        .and_local_timezone(Tz::UTC).single().ok_or("The device date is out of range.")?;
    let end_of_window = now.with_timezone(&Tz::UTC) + Duration::days(14);

    let mut exceptions: HashMap<&str, HashSet<i64>> = HashMap::new();
    for event in &events {
        if let Some(original) = event.original_start.as_ref().and_then(|stamp| stamp.date(zone)) {
            exceptions.entry(&event.uid).or_default().insert(original.timestamp());
        }
    }

    let mut tiles = Vec::new();
    for event in &events {
        let Some(stamp) = event.start.as_ref().filter(|_| !event.cancelled) else { continue };
        let Some(start) = stamp.date(zone) else { continue };
        let duration = event.end.as_ref().and_then(|end| end.date(zone))
            .map(|end| (end - start).max(Duration::zero()))
            .unwrap_or_else(|| if stamp.all_day() { Duration::days(1) } else { Duration::zero() });
        let since = if stamp.all_day() { today } else { now.with_timezone(&Tz::UTC) };
        for occurrence in occurrences(event, start, since - duration - Duration::seconds(1), end_of_window) {
            if occurrence > end_of_window || occurrence + duration < since
                || (stamp.all_day() && occurrence + duration == since)
                || (event.original_start.is_none() && exceptions.get(event.uid.as_str()).is_some_and(|dates| dates.contains(&occurrence.timestamp())))
            {
                continue;
            }
            let (sort, subtitle) = if stamp.all_day() {
                let day = occurrence.date_naive();
                (day.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(offset).unwrap().timestamp(),
                    format!("{} · All day", day.format("%a, %b %e")))
            } else {
                let local = occurrence.with_timezone(&offset);
                (local.timestamp(), local.format("%a, %b %e · %-I:%M %p").to_string())
            };
            tiles.push((sort, Item {
                id: format!("{}:{}", event.uid, occurrence.timestamp()),
                title: if event.title.trim().is_empty() { "Calendar event".into() } else { event.title.clone() },
                subtitle: Some(subtitle),
                row: Some(ROW.into()),
                ..Default::default()
            }));
        }
    }
    tiles.sort_by(|(a, first), (b, second)| a.cmp(b).then_with(|| first.id.cmp(&second.id)));
    let mut seen = HashSet::new();
    Ok(tiles.into_iter().map(|(_, item)| item)
        .filter(|item| seen.insert(item.id.clone())).take(MAX_ITEMS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_all_day_and_recurring_events_with_moved_and_cancelled_instances() {
        let feed = "BEGIN:VCALENDAR\nX-WR-TIMEZONE:America/New_York\n\
BEGIN:VEVENT\nUID:series\nDTSTART;TZID=America/New_York:20261118T190000\n\
RRULE:FREQ=DAILY;COUNT=5\nEXDATE;TZID=America/New_York:20261120T190000\nSUMMARY:Game night\nEND:VEVENT\n\
BEGIN:VEVENT\nUID:series\nRECURRENCE-ID;TZID=America/New_York:20261121T190000\n\
DTSTART;TZID=America/New_York:20261121T210000\nSUMMARY:Moved game\nEND:VEVENT\n\
BEGIN:VEVENT\nUID:series\nRECURRENCE-ID;TZID=America/New_York:20261122T190000\nSTATUS:CANCELLED\nEND:VEVENT\n\
BEGIN:VEVENT\nUID:holiday\nDTSTART;VALUE=DATE:20261119\nDTEND;VALUE=DATE:20261120\n\
SUMMARY:Day off\\, at\n home\nEND:VEVENT\nEND:VCALENDAR\n";
        let now = DateTime::parse_from_rfc3339("2026-11-19T15:00:00Z").unwrap().timestamp_millis();
        let items = upcoming(feed, now, -300).unwrap();
        let titles: Vec<_> = items.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(titles, ["Day off, athome", "Game night", "Moved game"]);
        assert!(items[0].subtitle.as_deref().unwrap().ends_with("All day"));
        assert!(items[2].subtitle.as_deref().unwrap().ends_with("9:00 PM"));
    }

    #[test]
    fn accepts_only_google_private_ical_addresses() {
        assert!(valid_address("https://calendar.google.com/calendar/ical/user%40example.org/private-123/basic.ics"));
        assert!(!valid_address("https://example.org/calendar/ical/user/private-123/basic.ics"));
        assert!(!valid_address("https://calendar.google.com/calendar/ical/user/public/basic.ics"));
    }
}
