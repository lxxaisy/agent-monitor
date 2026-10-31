//! Public announcement metadata only. No account credentials are sent to this source.
use crate::{passport::AlertRecord, storage::SnapshotStore};
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, NaiveDate, TimeZone, Utc};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    sync::{Arc, Mutex},
    time::Duration,
};

pub const SOURCE_URL: &str = "https://codex-reset.com/api/feed";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NewsSettings {
    pub enabled: bool,
    pub interval_minutes: u16,
}
impl Default for NewsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: 5,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetPost {
    pub id: String,
    pub url: String,
    pub text: String,
    pub published_at: i64,
    pub state: String,
    pub timing_text: String,
    #[serde(default)]
    pub timing_at: Option<i64>,
    #[serde(default)]
    pub timing_kind: Option<String>,
    #[serde(default)]
    pub timing_source_id: Option<String>,
    pub version: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsSnapshot {
    pub settings: NewsSettings,
    pub state: String,
    pub detail: String,
    pub source_url: String,
    pub checked_at: Option<i64>,
    pub source_updated_at: Option<i64>,
    pub next_check_at: i64,
    pub baseline_at: Option<i64>,
    pub posts: Vec<ResetPost>,
    pub failures: u32,
}
pub struct ResetNews {
    client: Client,
    store: Arc<SnapshotStore>,
    inner: Mutex<NewsSnapshot>,
    poll_lock: Mutex<()>,
}
fn timestamp(value: &Value) -> Option<i64> {
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|v| v.timestamp())
}

fn source_offset(text: &str) -> Option<FixedOffset> {
    let lower = text.to_ascii_lowercase();
    let seconds = if lower.contains("pdt") {
        -7 * 60 * 60
    } else if lower.contains("pst") || lower.contains(" pt") || lower.ends_with("pt") {
        -8 * 60 * 60
    } else {
        return None;
    };
    FixedOffset::east_opt(seconds)
}

fn clock_token(value: &str) -> Option<(u32, u32)> {
    let token = value
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != ':');
    let lower = token.to_ascii_lowercase();
    match lower.as_str() {
        "midnight" => return Some((0, 0)),
        "noon" => return Some((12, 0)),
        _ => {}
    }
    let (digits, suffix) = if let Some(value) = lower.strip_suffix("am") {
        (value, Some("am"))
    } else if let Some(value) = lower.strip_suffix("pm") {
        (value, Some("pm"))
    } else {
        (lower.as_str(), None)
    };
    let mut parts = digits.split(':');
    let hour = parts.next()?.parse::<u32>().ok()?;
    let minute = parts.next().unwrap_or("0").parse::<u32>().ok()?;
    if parts.next().is_some() || minute > 59 {
        return None;
    }
    let hour = match suffix {
        Some("am") if hour == 12 => 0,
        Some("pm") if hour < 12 => hour + 12,
        Some(_) if hour > 12 => return None,
        None if hour > 23 => return None,
        _ => hour,
    };
    Some((hour, minute))
}

fn local_timestamp(date: NaiveDate, hour: u32, minute: u32, offset: FixedOffset) -> Option<i64> {
    offset
        .from_local_datetime(&date.and_hms_opt(hour, minute, 0)?)
        .single()
        .map(|value| value.timestamp())
}

/// Extract only explicit timing language from a Tibo post or reply. Relative
/// phrases stay relative to the source post; no account-specific reset state is inferred.
fn timing_from_text(text: &str, published_at: i64) -> Option<(i64, String)> {
    let lower = text.to_ascii_lowercase();
    let published = DateTime::from_timestamp(published_at, 0)?;

    if lower.contains("within") || lower.contains(" in ") || lower.contains("next ") {
        let tokens: Vec<&str> = lower.split_whitespace().collect();
        for index in 0..tokens.len() {
            let is_hours = tokens[index] == "hour" || tokens[index] == "hours";
            let is_minutes = tokens[index] == "minute" || tokens[index] == "minutes";
            if !is_hours && !is_minutes {
                continue;
            }
            let amount = match tokens.get(index.saturating_sub(1)).copied() {
                Some("an") | Some("a") => 1,
                Some(value) => value.parse::<i64>().ok()?,
                None => continue,
            };
            let multiplier = if is_hours { 60 * 60 } else { 60 };
            if amount > 0 && amount <= 72 {
                return Some((published.timestamp() + amount * multiplier, "window".into()));
            }
        }
    }

    let offset = source_offset(&lower)?;
    let source_date = published.with_timezone(&offset).date_naive();
    let day_offset = if lower.contains("tomorrow") { 1 } else { 0 };
    let date = source_date.checked_add_signed(ChronoDuration::days(day_offset))?;
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    for index in 0..tokens.len() {
        if !matches!(tokens[index], "before" | "by" | "at") {
            continue;
        }
        let Some(clock) = tokens.get(index + 1).and_then(|value| clock_token(value)) else {
            continue;
        };
        if tokens.get(index + 2).is_some_and(|value| {
            value.trim_matches(|character: char| !character.is_ascii_alphabetic()) == "pst"
                || value.trim_matches(|character: char| !character.is_ascii_alphabetic()) == "pdt"
                || value.trim_matches(|character: char| !character.is_ascii_alphabetic()) == "pt"
        }) {
            let kind = if tokens[index] == "before" || tokens[index] == "by" {
                "deadline"
            } else {
                "point"
            };
            // Midnight closes the source's stated day. Anchor it to the post's
            // date in its explicit timezone, never to the machine's current day.
            let clock_date = if tokens[index + 1] == "midnight" {
                date.succ_opt()?
            } else {
                date
            };
            return local_timestamp(clock_date, clock.0, clock.1, offset)
                .map(|value| (value, kind.into()));
        }
    }

    if lower.contains("eod") {
        return local_timestamp(date, 23, 59, offset).map(|value| (value, "deadline".into()));
    }
    None
}

fn event_timing(feed: &Value, id: &str) -> Option<(i64, String)> {
    feed["events"].as_array()?.iter().find_map(|event| {
        if event["id"].as_str() != Some(id) {
            return None;
        }
        if let Some(value) = timestamp(&event["effective_at"]) {
            return Some((value, "point".into()));
        }
        let window = &event["official_window"];
        let value = timestamp(&window["target_at"])
            .or_else(|| timestamp(&window["end_at"]))
            .or_else(|| timestamp(&window["start_at"]))?;
        Some((value, "deadline".into()))
    })
}

fn reply_timing(tweets: &[Value], parent_id: &str) -> Option<(String, i64, String, String)> {
    tweets
        .iter()
        .filter(|tweet| {
            tweet["is_reply"].as_bool() == Some(true)
                && tweet["replying_to"].as_str() == Some("thsottiaux")
                && tweet["in_reply_to_tweet_id"].as_str() == Some(parent_id)
        })
        .filter_map(|tweet| {
            let text = tweet["text"].as_str()?.to_string();
            let at = timestamp(&tweet["at"])?;
            let (timing_at, kind) = timing_from_text(&text, at)?;
            let id = tweet["id"].as_str()?.to_string();
            Some((text, timing_at, id, kind))
        })
        .max_by_key(|(_, timing_at, _, _)| *timing_at)
}
pub fn state_label(state: &str) -> &'static str {
    match state {
        "confirmed" => "原帖称已重置",
        "banked" => "重置卡消息",
        _ => "重置公告待确认",
    }
}
fn parse_feed(feed: &Value, now: i64) -> Result<(i64, Vec<ResetPost>), String> {
    if feed["version"] != 1 || feed["profile"]["handle"] != "thsottiaux" {
        return Err("公告来源或协议不匹配".into());
    }
    let updated = timestamp(&feed["fetched_at"]).ok_or("公告来源缺少更新时间")?;
    if feed["stale"] != false || now - updated > 900 || updated > now + 60 {
        return Err("公告来源已过期，保留历史记录".into());
    }
    let tweets = feed["tweets"].as_array().ok_or("公告来源缺少原帖记录")?;
    let mut posts = Vec::new();
    for tweet in tweets {
        if tweet["tibo_lane"] != "reset_related"
            || !matches!(
                tweet["kind"].as_str(),
                Some("signal" | "candidate" | "banked")
            )
        {
            continue;
        }
        let id = tweet["id"].as_str().ok_or("公告缺少原帖 ID")?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err("公告 ID 不合法".into());
        }
        let url = format!("https://x.com/thsottiaux/status/{id}");
        if tweet["url"] != url {
            return Err("公告原帖链接不属于指定账号".into());
        }
        let text = tweet["text"].as_str().ok_or("公告缺少原文")?.to_string();
        if text.len() > 16000 {
            return Err("公告原文超过长度限制".into());
        }
        let published_at = timestamp(&tweet["at"]).ok_or("公告缺少发布时间")?;
        if published_at > now + 60 {
            return Err("公告发布时间晚于当前时间".into());
        }
        let lower = text.to_lowercase();
        // A deadline or a promise never becomes completed merely because time passes.
        let state = if tweet["kind"] == "banked" {
            "banked"
        } else if [
            "reset has been processed",
            "reset all propagated",
            "resets all propagated",
            "reset has landed",
            "have reset usage",
            "have now reset",
            "usage limits have been reset",
            "reset in the bag. all propagated",
            "side of reset? it's done.",
        ]
        .iter()
        .any(|phrase| lower.contains(phrase))
        {
            "confirmed"
        } else {
            "announced"
        };
        let own_timing = if text.len() <= 160
            && [
                "before ", "within ", "eod", "tomorrow", "pm pst", "am pst", "pm pt", "am pt",
                "midnight", "noon",
            ]
            .iter()
            .any(|phrase| lower.contains(phrase))
        {
            timing_from_text(&text, published_at)
        } else {
            None
        };
        let reply = if own_timing.is_none() {
            reply_timing(tweets, id)
        } else {
            None
        };
        let (timing_text, timing_at, timing_source_id, timing_kind) =
            if let Some((reply_text, reply_at, reply_id, kind)) = reply {
                (reply_text, Some(reply_at), Some(reply_id), Some(kind))
            } else if let Some((value, kind)) = own_timing {
                (text.clone(), Some(value), Some(id.into()), Some(kind))
            } else if let Some((value, kind)) = event_timing(feed, id) {
                (
                    "来源窗口提供的预计时间".into(),
                    Some(value),
                    None,
                    Some(kind),
                )
            } else {
                ("来源未提供可精确换算的重置时间".into(), None, None, None)
            };
        // Timing metadata can be enriched when the feed starts exposing a
        // reply. Keep the durable content version tied to the post itself so
        // that this enrichment does not replay every historical alert.
        let version = format!("{:x}", Sha256::digest(format!("{id}:{text}").as_bytes()));
        posts.push(ResetPost {
            id: id.into(),
            url,
            text,
            published_at,
            state: state.into(),
            timing_text,
            timing_at,
            timing_kind,
            timing_source_id,
            version,
        });
    }
    posts.sort_by_key(|post| std::cmp::Reverse(post.published_at));
    posts.dedup_by(|a, b| a.id == b.id);
    Ok((updated, posts))
}
impl ResetNews {
    pub fn new(store: Arc<SnapshotStore>) -> Result<Self, String> {
        let snapshot = match store.load_reset_monitor().map_err(|e| e.to_string())? {
            Some(value) => serde_json::from_value(value).map_err(|_| "公告本地记录格式不可用")?,
            None => NewsSnapshot {
                settings: NewsSettings::default(),
                state: "unavailable".into(),
                detail: "等待首次检查；首次回填不提醒".into(),
                source_url: SOURCE_URL.into(),
                checked_at: None,
                source_updated_at: None,
                next_check_at: 0,
                baseline_at: None,
                posts: Vec::new(),
                failures: 0,
            },
        };
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("Agent-Monitor/0.3.0")
                .build()
                .map_err(|_| "无法初始化公告连接")?,
            store,
            inner: Mutex::new(snapshot),
            poll_lock: Mutex::new(()),
        })
    }
    pub fn snapshot(&self) -> Result<NewsSnapshot, String> {
        let mut value = self.inner.lock().map_err(|_| "公告状态不可用")?.clone();
        if !value.settings.enabled {
            value.state = "disabled".into();
        } else if value
            .source_updated_at
            .is_some_and(|at| Utc::now().timestamp() - at > 900)
        {
            value.state = "stale".into();
            value.detail = "来源超过 15 分钟未更新；保留历史".into();
        }
        Ok(value)
    }
    pub fn settings(&self, settings: NewsSettings) -> Result<NewsSnapshot, String> {
        if !(1..=60).contains(&settings.interval_minutes) {
            return Err("检查间隔为 1–60 整数分钟".into());
        }
        let _poll = self.poll_lock.lock().map_err(|_| "公告检查不可用")?;
        let mut inner = self.inner.lock().map_err(|_| "公告状态不可用")?;
        inner.settings = settings;
        inner.next_check_at = Utc::now().timestamp();
        self.store
            .save_reset_monitor(&serde_json::to_value(&*inner).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        drop(inner);
        self.snapshot()
    }
    pub fn refresh(&self, force: bool) -> Result<Vec<AlertRecord>, String> {
        let Ok(_poll) = self.poll_lock.try_lock() else {
            return Ok(Vec::new());
        };
        let current = self.snapshot()?;
        let now = Utc::now().timestamp();
        if !current.settings.enabled || (!force && now < current.next_check_at) {
            return Ok(Vec::new());
        }
        let result = (|| {
            let response = self
                .client
                .get(SOURCE_URL)
                .send()
                .map_err(|_| "公告请求失败，请检查网络")?;
            if !response.status().is_success() {
                return Err(format!("公告请求返回 HTTP {}", response.status().as_u16()));
            }
            let mut bytes = Vec::new();
            response
                .take(1_048_577)
                .read_to_end(&mut bytes)
                .map_err(|_| "无法读取公告响应")?;
            if bytes.len() > 1_048_576 {
                return Err("公告响应超过 1 MiB".into());
            }
            let feed: Value =
                serde_json::from_slice(&bytes).map_err(|_| "公告响应不是有效 JSON")?;
            parse_feed(&feed, now)
        })();
        self.apply(result, now)
    }
    fn apply(
        &self,
        result: Result<(i64, Vec<ResetPost>), String>,
        now: i64,
    ) -> Result<Vec<AlertRecord>, String> {
        let mut inner = self.inner.lock().map_err(|_| "公告状态不可用")?;
        inner.checked_at = Some(now);
        let mut fresh = Vec::new();
        match result {
            Ok((updated, posts)) if inner.source_updated_at.map_or(true, |old| updated >= old) => {
                for post in posts {
                    let known = inner.posts.iter().find(|old| old.id == post.id);
                    let changed = known.is_some_and(|old| old.version != post.version);
                    let new = known.is_none()
                        && inner.baseline_at.is_some_and(|at| post.published_at > at);
                    if inner.baseline_at.is_some() && (changed || new) {
                        let alert = AlertRecord {
                            id: format!("reset:{}:{}", post.id, &post.version[..16]),
                            task_id: format!("reset:{}", post.id),
                            kind: "reset_news".into(),
                            title: format!("Tibo · {}", state_label(&post.state)),
                            created_at: Utc::now().to_rfc3339(),
                            unread: true,
                        };
                        if self
                            .store
                            .insert_alert_once(&alert)
                            .map_err(|e| e.to_string())?
                        {
                            fresh.push(alert);
                        }
                    }
                    inner.posts.retain(|old| old.id != post.id);
                    inner.posts.push(post);
                }
                inner
                    .posts
                    .sort_by_key(|p| std::cmp::Reverse(p.published_at));
                inner.posts.truncate(100);
                if inner.baseline_at.is_none() {
                    inner.baseline_at = Some(now);
                }
                inner.source_updated_at = Some(updated);
                inner.failures = 0;
                inner.state = "available".into();
                inner.detail =
                    "第三方转录 codex-reset.com；原帖 @thsottiaux；不代表当前账户已重置".into();
            }
            other => {
                inner.failures = inner.failures.saturating_add(1);
                inner.state = "error".into();
                inner.detail = other
                    .err()
                    .unwrap_or_else(|| "来源返回较旧快照；未覆盖当前记录".into());
            }
        }
        let multiplier = 1i64 << inner.failures.min(6);
        inner.next_check_at =
            now + (i64::from(inner.settings.interval_minutes) * 60 * multiplier).min(3600);
        self.store
            .save_reset_monitor(&serde_json::to_value(&*inner).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn feed(text: &str, now: i64) -> Value {
        json!({"version":1,"profile":{"handle":"thsottiaux"},"fetched_at":DateTime::from_timestamp(now,0).unwrap().to_rfc3339(),"stale":false,"tweets":[{"id":"123","url":"https://x.com/thsottiaux/status/123","text":text,"at":DateTime::from_timestamp(now,0).unwrap().to_rfc3339(),"tibo_lane":"reset_related","kind":"signal"}],"events":[{"observation_result":"reset_observed"}]})
    }
    #[test]
    fn promise_and_operator_observation_never_confirm_account() {
        let v = feed("Lands tomorrow before 10am PST.", 1000);
        let (_, posts) = parse_feed(&v, 1000).unwrap();
        assert_eq!(posts[0].state, "announced");
        assert_eq!(posts[0].published_at, 1000);
        assert_eq!(posts[0].timing_text, "Lands tomorrow before 10am PST.");
        assert!(posts[0].timing_at.is_some());
        assert!(parse_feed(&v, 2000).is_err());
        let mut v = v;
        v["profile"]["handle"] = json!("other");
        assert!(parse_feed(&v, 1000).is_err());
    }

    #[test]
    fn attaches_the_first_timing_reply_to_the_original_post() {
        let mut value = feed("Day 18/ Reset", 1_000);
        value["tweets"][0]["is_reply"] = json!(false);
        value["tweets"][0]["kind"] = json!("candidate");
        value["tweets"][0]["in_reply_to_tweet_id"] = Value::Null;
        value["tweets"].as_array_mut().unwrap().push(json!({
            "id":"456",
            "url":"https://x.com/thsottiaux/status/456",
            "text":"Landing tomorrow before 10am PST.",
            "at":"1970-01-01T00:16:40Z",
            "is_reply":true,
            "replying_to":"thsottiaux",
            "in_reply_to_tweet_id":"123",
            "tibo_lane":"reset_related",
            "kind":"signal"
        }));
        let (_, posts) = parse_feed(&value, 1_000).unwrap();
        let original = posts.iter().find(|post| post.id == "123").unwrap();
        assert_eq!(original.timing_text, "Landing tomorrow before 10am PST.");
        assert_eq!(original.timing_source_id.as_deref(), Some("456"));
        assert!(original.timing_at.is_some());
    }

    #[test]
    fn parses_relative_hour_and_minute_windows_without_inventing_a_timezone() {
        assert_eq!(
            timing_from_text("Landing within 30 minutes", 1_000).map(|(value, kind)| (value, kind)),
            Some((2_800, "window".into()))
        );
        assert_eq!(
            timing_from_text("Landing in 2 hours", 1_000).map(|(value, kind)| (value, kind)),
            Some((8_200, "window".into()))
        );
        assert_eq!(timing_from_text("Sometime tomorrow", 1_000), None);
    }

    #[test]
    fn midnight_pst_reply_has_a_localizable_deadline() {
        let published = DateTime::parse_from_rfc3339("2026-10-25T03:40:04Z")
            .unwrap()
            .timestamp();
        let expected = DateTime::parse_from_rfc3339("2026-10-25T16:00:00+08:00")
            .unwrap()
            .timestamp();
        let (_, posts) = parse_feed(&feed("By midnight PST", published), published).unwrap();
        assert_eq!(posts[0].timing_at, Some(expected));
        assert_eq!(posts[0].timing_text, "By midnight PST");
        assert_eq!(posts[0].timing_kind.as_deref(), Some("deadline"));
        assert_eq!(posts[0].state, "announced");
    }

    #[test]
    fn named_clocks_keep_explicit_offsets_and_source_calendar_boundaries() {
        for (text, published, expected) in [
            (
                "By midnight PST",
                "2026-12-31T20:00:00-08:00",
                "2027-01-01T16:00:00+08:00",
            ),
            (
                "Before midnight PDT",
                "2026-10-24T20:00:00-07:00",
                "2026-10-25T15:00:00+08:00",
            ),
            (
                "At noon PST",
                "2026-10-24T08:00:00-08:00",
                "2026-10-25T04:00:00+08:00",
            ),
        ] {
            let published = DateTime::parse_from_rfc3339(published).unwrap().timestamp();
            let expected = DateTime::parse_from_rfc3339(expected).unwrap().timestamp();
            assert_eq!(
                timing_from_text(text, published).map(|value| value.0),
                Some(expected),
                "{text}"
            );
        }
        assert_eq!(timing_from_text("By midnight", 1_000), None);
    }

    #[test]
    fn midnight_reply_enriches_parent_without_replaying_an_alert() {
        let at = DateTime::parse_from_rfc3339("2026-10-25T03:39:31Z")
            .unwrap()
            .timestamp();
        let mut value = feed("Day 20/ Rest and reset", at);
        let (_, original) = parse_feed(&value, at).unwrap();
        value["tweets"].as_array_mut().unwrap().push(json!({
            "id":"456", "url":"https://x.com/thsottiaux/status/456",
            "text":"By midnight PST", "at":"2026-10-25T03:40:04Z",
            "is_reply":true, "replying_to":"thsottiaux", "in_reply_to_tweet_id":"123",
            "tibo_lane":"reset_related", "kind":"signal"
        }));
        let (_, posts) = parse_feed(&value, at + 60).unwrap();
        let parent = posts.iter().find(|post| post.id == "123").unwrap();
        let reply = posts.iter().find(|post| post.id == "456").unwrap();
        assert_eq!(parent.timing_source_id.as_deref(), Some("456"));
        assert_eq!(parent.timing_at, reply.timing_at);
        assert_eq!(parent.timing_kind.as_deref(), Some("deadline"));
        assert_eq!(parent.version, original[0].version);
    }
    #[test]
    fn explicit_completion_wording_from_original_posts_is_labeled_as_a_claim() {
        for text in [
            "Put the reset in the bag. All propagated.",
            "What would you like with your side of reset? It's done.",
        ] {
            assert_eq!(
                parse_feed(&feed(text, 1000), 1000).unwrap().1[0].state,
                "confirmed"
            );
        }
    }
    #[test]
    fn baseline_dedupe_edits_restart_and_failure() {
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let service = ResetNews::new(store.clone()).unwrap();
        assert!(service
            .apply(parse_feed(&feed("Reset today", 1000), 1000), 1000)
            .unwrap()
            .is_empty());
        assert!(service
            .apply(parse_feed(&feed("Reset today", 1000), 1000), 1001)
            .unwrap()
            .is_empty());
        assert_eq!(
            service
                .apply(parse_feed(&feed("Reset all propagated", 1000), 1000), 1002)
                .unwrap()
                .len(),
            1
        );
        let restarted = ResetNews::new(store).unwrap();
        assert!(restarted
            .apply(parse_feed(&feed("Reset all propagated", 1000), 1000), 1003)
            .unwrap()
            .is_empty());
        restarted.apply(Err("offline".into()), 1004).unwrap();
        let state = restarted.inner.lock().unwrap();
        assert_eq!(state.posts.len(), 1);
        assert_eq!(state.posts[0].state, "confirmed");
        assert_eq!(state.next_check_at, 1604);
    }

    #[test]
    fn new_posts_notify_but_historical_backfill_and_disabled_poll_do_not() {
        let store = Arc::new(SnapshotStore::open_in_memory().unwrap());
        let service = ResetNews::new(store.clone()).unwrap();
        service
            .apply(parse_feed(&feed("Reset today", 1000), 1000), 1000)
            .unwrap();
        let mut next = feed("New reset announced", 1002);
        next["tweets"][0]["id"] = json!("456");
        next["tweets"][0]["url"] = json!("https://x.com/thsottiaux/status/456");
        assert_eq!(
            service.apply(parse_feed(&next, 1002), 1002).unwrap().len(),
            1
        );
        next["tweets"][0]["id"] = json!("789");
        next["tweets"][0]["url"] = json!("https://x.com/thsottiaux/status/789");
        next["tweets"][0]["at"] = json!(DateTime::from_timestamp(999, 0).unwrap().to_rfc3339());
        assert!(service
            .apply(parse_feed(&next, 1002), 1003)
            .unwrap()
            .is_empty());
        assert_eq!(store.load_alerts().unwrap().len(), 1);
        service
            .settings(NewsSettings {
                enabled: false,
                interval_minutes: 7,
            })
            .unwrap();
        assert!(service.refresh(true).unwrap().is_empty());
        let restarted = ResetNews::new(store).unwrap();
        assert!(!restarted.snapshot().unwrap().settings.enabled);
        assert_eq!(restarted.snapshot().unwrap().settings.interval_minutes, 7);
        assert!(restarted
            .settings(NewsSettings {
                enabled: true,
                interval_minutes: 0
            })
            .is_err());
    }

    #[test]
    fn forged_links_future_dates_and_unrelated_posts_are_rejected_or_excluded() {
        let mut value = feed("Reset today", 1000);
        value["tweets"][0]["url"] = json!("https://example.com/123");
        assert!(parse_feed(&value, 1000).is_err());
        value = feed("Reset today", 1061);
        value["fetched_at"] = json!(DateTime::from_timestamp(1000, 0).unwrap().to_rfc3339());
        assert!(parse_feed(&value, 1000).is_err());
        value = feed("Unrelated", 1000);
        value["tweets"][0]["tibo_lane"] = json!("other");
        assert!(parse_feed(&value, 1000).unwrap().1.is_empty());
    }
}
