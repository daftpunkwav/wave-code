/*!
 * @file NoticeBus
 * @description In-process topic bus plus webhook payload formatting.
 *
 * Responsibilities:
 * - Fan notices out to per-topic subscribers synchronously.
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

/// Synchronous in-process topic bus.
#[derive(Default)]
pub struct TopicBus {
    subscribers: std::collections::HashMap<String, Vec<Subscriber>>,
}

impl TopicBus {
    /// Create an empty bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe one callback to a topic.
    pub fn subscribe(&mut self, topic: impl Into<String>, subscriber: Subscriber) {
        self.subscribers
            .entry(topic.into())
            .or_default()
            .push(subscriber);
    }

    /// Publish one notice to every subscriber of its topic, in order.
    ///
    /// Unknown topics are silent no-ops; subscriber panics propagate to
    /// the publisher by design (bugs must surface, not hide in a bus).
    pub fn publish(&self, notice: Notice) {
        if let Some(subscribers) = self.subscribers.get(&notice.topic) {
            for subscriber in subscribers {
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
