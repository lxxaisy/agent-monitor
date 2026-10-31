use crate::passport::AlertRecord;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DesktopNotice {
    pub id: String,
    pub task_id: String,
    pub kind: String,
    pub title: String,
    pub created_at: String,
    pub expires_at: String,
}

#[derive(Default)]
pub struct DesktopNotices(Mutex<Vec<DesktopNotice>>);

impl DesktopNotices {
    pub fn snapshot(&self) -> Result<Vec<DesktopNotice>, String> {
        Ok(self.0.lock().map_err(|_| "提醒状态不可用")?.clone())
    }

    pub fn publish(
        &self,
        alerts: &[AlertRecord],
        duration_seconds: u64,
        now: DateTime<Utc>,
    ) -> Result<Vec<DesktopNotice>, String> {
        let mut notices = self.0.lock().map_err(|_| "提醒状态不可用")?;
        for alert in alerts {
            if notices.iter().any(|notice| notice.id == alert.id) {
                continue;
            }
            notices.insert(
                0,
                DesktopNotice {
                    id: alert.id.clone(),
                    task_id: alert.task_id.clone(),
                    kind: alert.kind.clone(),
                    title: alert.title.clone(),
                    created_at: alert.created_at.clone(),
                    expires_at: (now + Duration::seconds(duration_seconds as i64)).to_rfc3339(),
                },
            );
        }
        Ok(notices.clone())
    }

    pub fn dismiss(&self, id: &str) -> Result<(DesktopNotice, Vec<DesktopNotice>), String> {
        let mut notices = self.0.lock().map_err(|_| "提醒状态不可用")?;
        let index = notices
            .iter()
            .position(|notice| notice.id == id)
            .ok_or("这条浮窗提醒已关闭或到期")?;
        let dismissed = notices.remove(index);
        Ok((dismissed, notices.clone()))
    }

    pub fn expire(&self, now: DateTime<Utc>) -> Result<Option<Vec<DesktopNotice>>, String> {
        let mut notices = self.0.lock().map_err(|_| "提醒状态不可用")?;
        let old_len = notices.len();
        notices.retain(|notice| {
            DateTime::parse_from_rfc3339(&notice.expires_at)
                .expect("notice expiry is generated internally")
                > now
        });
        Ok((notices.len() != old_len).then(|| notices.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(id: &str, kind: &str) -> AlertRecord {
        AlertRecord {
            id: id.into(),
            task_id: "codex:chat:turn".into(),
            kind: kind.into(),
            title: "A real task title".into(),
            created_at: "2026-10-22T02:00:00Z".into(),
            unread: true,
        }
    }

    #[test]
    fn retains_until_five_minutes_and_expires_at_deadline() {
        let notices = DesktopNotices::default();
        let now = DateTime::parse_from_rfc3339("2026-10-22T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        notices
            .publish(&[alert("input", "needs_action")], 300, now)
            .unwrap();
        assert!(notices
            .expire(now + Duration::seconds(299))
            .unwrap()
            .is_none());
        assert_eq!(notices.snapshot().unwrap().len(), 1);
        assert_eq!(
            notices.expire(now + Duration::seconds(300)).unwrap(),
            Some(vec![])
        );
    }

    #[test]
    fn independent_expiry_and_manual_close_preserve_other_notices_and_unread() {
        let store = crate::storage::SnapshotStore::open_in_memory().unwrap();
        let notices = DesktopNotices::default();
        let now = Utc::now();
        let input = alert("input", "needs_action");
        store.insert_alert_once(&input).unwrap();
        notices.publish(&[input.clone()], 300, now).unwrap();
        notices
            .publish(&[alert("done", "completed")], 60, now)
            .unwrap();
        assert_eq!(notices.snapshot().unwrap()[0].id, "done");
        assert_eq!(
            notices
                .expire(now + Duration::seconds(60))
                .unwrap()
                .unwrap()[0]
                .id,
            "input"
        );
        let (closed, remaining) = notices.dismiss("input").unwrap();
        assert_eq!(closed.task_id, input.task_id);
        assert!(remaining.is_empty());
        assert!(store.load_alerts().unwrap()[0].unread);
    }

    #[test]
    fn duplicate_delivery_does_not_extend_expiry_or_replace_other_notices() {
        let notices = DesktopNotices::default();
        let now = Utc::now();
        let original = notices
            .publish(&[alert("first", "needs_action")], 300, now)
            .unwrap();
        let updated = notices
            .publish(
                &[alert("first", "needs_action"), alert("second", "failed")],
                300,
                now + Duration::seconds(10),
            )
            .unwrap();
        assert_eq!(updated.len(), 2);
        assert_eq!(updated[1], original[0]);
    }
}
