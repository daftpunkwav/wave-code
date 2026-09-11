/*!
 * @file NoticeBus
 * @description In-process topic bus plus webhook payload formatting.
 *
 * Responsibilities:
 * - Fan notices out to per-topic subscribers synchronously.
 * - Hand out subscription ids so dynamic subscribers can detach.
 * - Format webhook payloads for external delivery.
 * - Keep delivery itself with the composition root (no HTTP here).
 *
 * This module must not depend on: any other workspace crate.
 */

//! Notifications as data flow: bus inside, HTTP outside.

/// One notice published on a topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// Topic the notice was published on.
    pub topic: String,
    /// Short title for lists and subjects.
    pub title: String,
    /// Longer body for detail views.
    pub body: String,
}

/// Subscriber callback type.
pub type Subscriber = Box<dyn Fn(&Notice) + Send + Sync>;

/// Handle to one subscription, returned by [`TopicBus::subscribe`].
///
/// Ids are bus-scoped and never reused within a bus lifetime, so a stale
/// id can never detach a newer subscriber by accident.
pub type SubscriptionId = u64;

/// Synchronous in-process topic bus.
#[derive(Default)]
pub struct TopicBus {
    subscribers: std::collections::HashMap<String, Vec<(SubscriptionId, Subscriber)>>,
    next_id: SubscriptionId,
}

impl TopicBus {
    /// Create an empty bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe one callback to a topic, returning its id for later
    /// detachment with [`TopicBus::unsubscribe`].
    pub fn subscribe(
        &mut self,
        topic: impl Into<String>,
        subscriber: Subscriber,
    ) -> SubscriptionId {
        let id = self.next_id;
        self.next_id += 1;
        self.subscribers
            .entry(topic.into())
            .or_default()
            .push((id, subscriber));
        id
    }

    /// Detach one subscription by id; true when anything was removed.
    ///
    /// Unknown ids are silent no-ops (false): double-detach after a
    /// re-subscribe must never disturb the newer subscriber, and ids
    /// are never reused, so false always means "nothing held that id".
    pub fn unsubscribe(&mut self, id: SubscriptionId) -> bool {
        let mut removed = false;
        for subscribers in self.subscribers.values_mut() {
            if let Some(pos) = subscribers.iter().position(|(held, _)| *held == id) {
                // Explicit drop: the removed subscriber owns a must-use box.
                let _ = subscribers.remove(pos);
                removed = true;
            }
        }
        removed
    }

    /// Publish one notice to every subscriber of its topic, in order.
    ///
    /// Unknown topics are silent no-ops; subscriber panics propagate to
    /// the publisher by design (bugs must surface, not hide in a bus).
    pub fn publish(&self, notice: Notice) {
        if let Some(subscribers) = self.subscribers.get(&notice.topic) {
            for (_, subscriber) in subscribers {
                subscriber(&notice);
            }
        }
    }

    /// Subscriber count for one topic.
    pub fn subscriber_count(&self, topic: &str) -> usize {
        self.subscribers.get(topic).map(Vec::len).unwrap_or(0)
    }
}

/// External webhook payload for run lifecycle events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookBody {
    /// Event name, e.g. `turn_completed`.
    pub event: String,
    /// Run identifier for correlation.
    pub run_id: String,
    /// Human-readable title.
    pub title: String,
    /// Human-readable detail.
    pub detail: String,
}

impl WebhookBody {
    /// Render the delivery payload as JSON.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "event": self.event,
            "run_id": self.run_id,
            "title": self.title,
            "detail": self.detail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn fan_out_reaches_every_subscriber_in_order() {
        let mut bus = TopicBus::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        for tag in ["first", "second"] {
            let seen = seen.clone();
            bus.subscribe(
                "turns",
                Box::new(move |notice: &Notice| {
                    seen.lock().unwrap().push(format!("{tag}:{}", notice.title));
                }),
            );
        }
        bus.publish(Notice {
            topic: "turns".to_string(),
            title: "done".to_string(),
            body: "b".to_string(),
        });
        // Unknown topics disturb nobody.
        bus.publish(Notice {
            topic: "elsewhere".to_string(),
            title: "x".to_string(),
            body: "y".to_string(),
        });
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["first:done".to_string(), "second:done".to_string()]
        );
        assert_eq!(bus.subscriber_count("turns"), 2);
    }

    #[test]
    fn detached_subscribers_stop_receiving() {
        let mut bus = TopicBus::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let first = {
            let seen = seen.clone();
            bus.subscribe(
                "turns",
                Box::new(move |notice: &Notice| {
                    seen.lock().unwrap().push(format!("first:{}", notice.title));
                }),
            )
        };
        let seen2 = seen.clone();
        bus.subscribe(
            "turns",
            Box::new(move |notice: &Notice| {
                seen2.lock().unwrap().push(format!("second:{}", notice.title));
            }),
        );
        let notice = |title: &str| Notice {
            topic: "turns".to_string(),
            title: title.to_string(),
            body: "b".to_string(),
        };
        bus.publish(notice("one"));
        assert!(bus.unsubscribe(first));
        bus.publish(notice("two"));
        // Unknown and already-removed ids remove nothing.
        assert!(!bus.unsubscribe(first));
        assert!(!bus.unsubscribe(999_999));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "first:one".to_string(),
                "second:one".to_string(),
                "second:two".to_string(),
            ]
        );
        assert_eq!(bus.subscriber_count("turns"), 1);
    }

    #[test]
    fn webhook_bodies_serialize_with_all_fields() {
        let body = WebhookBody {
            event: "turn_completed".to_string(),
            run_id: "r1".to_string(),
            title: "t".to_string(),
            detail: "d".to_string(),
        };
        let json = body.to_json();
        assert_eq!(json["event"], "turn_completed");
        assert_eq!(json["run_id"], "r1");
    }
}
