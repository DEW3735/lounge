use chrono::{DateTime, Duration, FixedOffset, Utc};
use lounge_plugin_sdk::{
    self as sdk, HttpRequest, Item, Kind, Manifest, RefreshContext, RowSpec, RowStyle,
    SettingKind, SettingSpec, host,
};
use serde::Deserialize;

struct Sports;
const ROW: &str = "scoreboard";
const LEAGUES: &[(&str, &str, &str)] = &[
    ("nfl", "NFL", "football/nfl"),
    ("nba", "NBA", "basketball/nba"),
    ("wnba", "WNBA", "basketball/wnba"),
    ("mlb", "MLB", "baseball/mlb"),
    ("nhl", "NHL", "hockey/nhl"),
    ("epl", "Premier League", "soccer/eng.1"),
    ("ucl", "Champions League", "soccer/uefa.champions"),
];

impl sdk::Plugin for Sports {
    fn manifest() -> Manifest {
        Manifest {
            id: "tv.lounge.dew-sports".into(),
            name: "Sports".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Today's and tomorrow's games with live scores and broadcast listings from ESPN.".into(),
            publisher_name: "DEW Lounge".into(),
            kinds: vec![Kind::Content],
            fetch_hosts: vec!["site.api.espn.com".into()],
            refresh_minutes: 5,
            rows: vec![RowSpec {
                id: ROW.into(), title: "Sports".into(), style: RowStyle::Compact,
            }],
            settings: LEAGUES.iter().map(|(key, label, _)| SettingSpec {
                key: (*key).into(), label: (*label).into(), kind: SettingKind::Toggle,
                required: false, help: None,
                default: Some(matches!(*key, "nfl" | "nba" | "mlb" | "nhl").to_string()),
            }).collect(),
            homepage: Some("https://www.espn.com/".into()),
            self_signed_servers: false,
            for_apps: Vec::new(),
            sent_to: Default::default(),
        }
    }

    fn refresh(ctx: &RefreshContext) -> Result<Vec<Item>, String> {
        let now = DateTime::<Utc>::from_timestamp_millis(ctx.now_ms).ok_or("The device clock is out of range.")?;
        let offset = FixedOffset::east_opt(ctx.utc_offset_minutes.saturating_mul(60))
            .ok_or("The device time zone is out of range.")?;
        let today = now.with_timezone(&offset).date_naive();
        let mut games = Vec::new();
        let mut failed = false;
        let mut loaded = false;
        for (key, label, path) in LEAGUES {
            if !host::setting(key).map_or(matches!(*key, "nfl" | "nba" | "mlb" | "nhl"), |v| v == "true") {
                continue;
            }
            for day in [today, today + Duration::days(1)] {
                let address = format!("https://site.api.espn.com/apis/site/v2/sports/{path}/scoreboard?dates={}&limit=50", day.format("%Y%m%d"));
                let mut request = HttpRequest::get(address);
                request.timeout_ms = 1_500;
                let Ok(response) = host::fetch(&request) else { failed = true; continue };
                if response.status != 200 {
                    failed = true;
                    continue;
                }
                let Ok(board) = serde_json::from_str::<Scoreboard>(&response.body) else {
                    failed = true;
                    continue;
                };
                loaded = true;
                games.extend(board.events.into_iter().filter_map(|game| game.tile(key, label, offset)));
            }
        }
        if failed && !loaded {
            return Err("The ESPN scoreboard is temporarily unavailable.".into());
        }
        if failed {
            sdk::warn!("Some ESPN scoreboards were unavailable.");
        }
        games.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.id.cmp(&b.2.id)));
        let mut seen = std::collections::HashSet::new();
        Ok(games.into_iter().map(|(_, _, item)| item)
            .filter(|item| seen.insert(item.id.clone())).take(24).enumerate().map(|(index, mut item)| {
                item.weight = -i32::try_from(index).expect("row size is bounded");
                item
            }).collect())
    }
}

sdk::export_plugin!(Sports);

#[derive(Deserialize)]
struct Scoreboard {
    events: Vec<Game>,
}

#[derive(Deserialize)]
struct Game {
    id: String,
    date: String,
    #[serde(rename = "shortName")]
    short_name: Option<String>,
    status: Status,
    #[serde(default)]
    competitions: Vec<Competition>,
}

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "type")]
    kind: StatusType,
}

#[derive(Deserialize)]
struct StatusType {
    state: String,
    name: String,
    #[serde(rename = "shortDetail", default)]
    detail: String,
}

#[derive(Deserialize)]
struct Competition {
    #[serde(default)]
    competitors: Vec<Competitor>,
    #[serde(default)]
    broadcasts: Vec<Broadcast>,
}

#[derive(Deserialize)]
struct Competitor {
    #[serde(rename = "homeAway")]
    side: String,
    score: Option<String>,
}

#[derive(Deserialize)]
struct Broadcast {
    #[serde(default)]
    names: Vec<String>,
}

impl Game {
    fn tile(self, key: &str, league: &str, offset: FixedOffset) -> Option<(u8, i64, Item)> {
        let start = DateTime::parse_from_rfc3339(&self.date).ok().or_else(|| {
            let without_zone = self.date.strip_suffix('Z')?;
            DateTime::parse_from_rfc3339(&format!("{without_zone}:00Z")).ok()
        })?.with_timezone(&offset);
        let title = self.short_name.filter(|name| !name.trim().is_empty())?;
        let status = self.status.kind;
        let withdrawn = ["POSTPON", "CANCEL", "SUSPEND"].iter().any(|word| status.name.contains(word));
        let competition = self.competitions.first();
        let score = |side| competition?.competitors.iter().find(|team| team.side == side)?.score.as_deref();
        let scores = score("away").zip(score("home")).map(|(away, home)| format!("{away}–{home}"));
        let station = competition.and_then(|c| c.broadcasts.iter().flat_map(|b| &b.names).next());
        let (priority, time, detail) = match status.state.as_str() {
            "in" if !withdrawn => (0, start.timestamp(), format!("LIVE · {}", status.detail)),
            "pre" if !withdrawn => (1, start.timestamp(), start.format("%a · %-I:%M %p").to_string()),
            _ => (2, -start.timestamp(), status.detail),
        };
        let mut subtitle = vec![league.to_owned(), detail];
        if priority != 1 {
            subtitle.extend(scores);
        }
        subtitle.extend(station.cloned());
        Some((priority, time, Item {
            id: format!("{key}:{}", self.id),
            title,
            subtitle: Some(subtitle.join(" · ")),
            row: Some(ROW.into()),
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_game_shows_score_and_broadcaster() {
        let game: Game = serde_json::from_str(r#"{
            "id":"10","date":"2026-11-19T23:30Z","shortName":"NY @ BOS",
            "status":{"type":{"state":"in","name":"STATUS_IN_PROGRESS","shortDetail":"3rd"}},
            "competitions":[{"competitors":[{"homeAway":"home","score":"82"},{"homeAway":"away","score":"76"}],
                "broadcasts":[{"names":["ESPN"]}]}]
        }"#).unwrap();
        let (priority, _, tile) = game.tile("nba", "NBA", FixedOffset::east_opt(-18_000).unwrap()).unwrap();
        assert_eq!(priority, 0);
        assert_eq!(tile.subtitle.as_deref(), Some("NBA · LIVE · 3rd · 76–82 · ESPN"));
    }
}
