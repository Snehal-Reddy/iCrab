//! Tick loop: load jobs.json, find due jobs, execute (inbound to agent or direct sendMessage).

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::telegram::{InboundMsg, OutboundMsg};
use crate::tools::cron::{CronStore, JobAction};

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run one tick: find due jobs, send to channels, mark fired. Used by runner and tests.
pub async fn tick_once(
    store: &CronStore,
    inbound_tx: &mpsc::Sender<InboundMsg>,
    outbound_tx: &mpsc::Sender<OutboundMsg>,
    now: u64,
) {
    let due = store.find_due(now);
    for job in due {
        match job.action {
            JobAction::Agent => {
                let msg = InboundMsg {
                    chat_id: job.chat_id,
                    user_id: 0,
                    text: job.message.clone(),
                    channel: "cron".to_string(),
                };
                if inbound_tx.try_send(msg).is_err() {
                    eprintln!("cron runner: inbound channel full, dropping agent job {}", job.id);
                }
            }
            JobAction::Direct => {
                let msg = OutboundMsg {
                    chat_id: job.chat_id,
                    text: job.message.clone(),
                    channel: "cron".to_string(),
                };
                if outbound_tx.try_send(msg).is_err() {
                    eprintln!("cron runner: outbound channel full, dropping direct job {}", job.id);
                }
            }
        }
        store.mark_fired(&job.id, now);
    }
}

async fn tick_loop(
    store: Arc<CronStore>,
    inbound_tx: mpsc::Sender<InboundMsg>,
    outbound_tx: mpsc::Sender<OutboundMsg>,
    tick_secs: u64,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(tick_secs));
    interval.tick().await;
    loop {
        interval.tick().await;
        let now = unix_now();
        tick_once(&store, &inbound_tx, &outbound_tx, now).await;
    }
}

/// Spawns the cron runner task. Returns the join handle (caller may ignore).
pub fn spawn_cron_runner(
    store: Arc<CronStore>,
    inbound_tx: mpsc::Sender<InboundMsg>,
    outbound_tx: mpsc::Sender<OutboundMsg>,
    tick_interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tick_loop(store, inbound_tx, outbound_tx, tick_interval_secs).await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::cron::{CronStore, Schedule};

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn tick_fires_due_direct_job() {
        let dir = std::env::temp_dir().join("icrab_cron_runner_direct");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = CronStore::empty(&dir);
        let base = unix_now();
        store
            .add(
                None,
                "Reminder".to_string(),
                JobAction::Direct,
                Schedule::Once { at_unix: base + 60 },
                12345,
            )
            .unwrap();
        let (inbound_tx, _inbound_rx) = mpsc::channel(8);
        let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
        tick_once(&store, &inbound_tx, &outbound_tx, base + 61).await;
        let msg = outbound_rx.try_recv().unwrap();
        assert_eq!(msg.chat_id, 12345);
        assert_eq!(msg.text, "Reminder");
        assert_eq!(msg.channel, "cron");
        let job = store.get("job-1").unwrap();
        assert!(job.last_run.is_some());
        assert!(!job.enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tick_fires_due_agent_job() {
        let dir = std::env::temp_dir().join("icrab_cron_runner_agent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = CronStore::empty(&dir);
        let base = unix_now();
        store
            .add(
                None,
                "Agent task".to_string(),
                JobAction::Agent,
                Schedule::Once { at_unix: base + 60 },
                999,
            )
            .unwrap();
        let (inbound_tx, mut inbound_rx) = mpsc::channel(8);
        let (outbound_tx, _outbound_rx) = mpsc::channel(8);
        tick_once(&store, &inbound_tx, &outbound_tx, base + 61).await;
        let msg = inbound_rx.try_recv().unwrap();
        assert_eq!(msg.chat_id, 999);
        assert_eq!(msg.text, "Agent task");
        assert_eq!(msg.channel, "cron");
        assert_eq!(msg.user_id, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tick_skips_not_due() {
        let dir = std::env::temp_dir().join("icrab_cron_runner_skip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = CronStore::empty(&dir);
        let base = unix_now();
        store
            .add(
                None,
                "Later".to_string(),
                JobAction::Direct,
                Schedule::Once { at_unix: base + 1000 },
                1,
            )
            .unwrap();
        let (inbound_tx, _inbound_rx) = mpsc::channel(8);
        let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
        tick_once(&store, &inbound_tx, &outbound_tx, base + 500).await;
        assert!(outbound_rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
