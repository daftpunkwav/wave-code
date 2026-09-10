/*!
 * @file ModelRouter
 * @description Priority model routes with fallback chains and cost books.
 *
 * Responsibilities:
 * - Pick the highest-priority route for sampling attempts.
 * - Expose the ordered fallback chain for retries.
 * - Track spend per model from reported token usage.
 *
 * This module must not depend on: any other workspace crate. Provider
 * clients stay behind the model gateway; routing only names models.
 */

//! Routing as ordered preference plus honest cost math.

use std::collections::HashMap;

/// One candidate model route.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRoute {
    /// Provider model identifier.
    pub model: String,
    /// Lower picks first; ties keep registration order.
    pub priority: u8,
    /// Price per 1k input tokens in caller currency units.
    pub price_in_per_1k: f64,
    /// Price per 1k output tokens in caller currency units.
    pub price_out_per_1k: f64,
}

/// Ordered router over candidate routes.
#[derive(Debug, Clone, Default)]
pub struct Router {
    routes: Vec<ModelRoute>,
}

impl Router {
    /// Create an empty router (picks nothing until routes register).
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one route.
    pub fn add_route(&mut self, route: ModelRoute) {
        self.routes.push(route);
    }

    /// Ordered routes: priority ascending, registration order on ties.
    pub fn chain(&self) -> Vec<&ModelRoute> {
        let mut indexed: Vec<(usize, &ModelRoute)> = self.routes.iter().enumerate().collect();
        indexed.sort_by_key(|(i, r)| (r.priority, *i));
        indexed.into_iter().map(|(_, r)| r).collect()
    }

    /// First route of the chain, if any route is registered.
    pub fn pick(&self) -> Option<&ModelRoute> {
        self.chain().into_iter().next()
    }
}

/// Routing errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoutingError {
    /// No price registered for the model.
    #[error("no price registered for model: {0}")]
    UnknownModel(String),
}

/// Spend tracker fed by settled token usage.
#[derive(Debug, Clone, Default)]
pub struct CostTracker {
    prices: HashMap<String, (f64, f64)>,
    spend: f64,
}

impl CostTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register per-1k input/output prices for one model.
    pub fn set_price(
        &mut self,
        model: impl Into<String>,
        price_in_per_1k: f64,
        price_out_per_1k: f64,
    ) {
        self.prices
            .insert(model.into(), (price_in_per_1k, price_out_per_1k));
    }

    /// Add spend for reported usage; unknown models fail explicitly
    /// instead of silently booking zero cost.
    pub fn record(
        &mut self,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<f64, RoutingError> {
        let (price_in, price_out) = self
            .prices
            .get(model)
            .copied()
            .ok_or_else(|| RoutingError::UnknownModel(model.to_string()))?;
        let cost =
            input_tokens as f64 / 1000.0 * price_in + output_tokens as f64 / 1000.0 * price_out;
        self.spend += cost;
        Ok(cost)
    }

    /// Total booked spend across all recordings.
    pub fn spend(&self) -> f64 {
        self.spend
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_orders_by_priority_with_stable_ties() {
        let mut router = Router::new();
        assert!(router.pick().is_none());
        router.add_route(ModelRoute {
            model: "b".to_string(),
            priority: 1,
            price_in_per_1k: 0.0,
            price_out_per_1k: 0.0,
        });
        router.add_route(ModelRoute {
            model: "a".to_string(),
            priority: 0,
            price_in_per_1k: 0.0,
            price_out_per_1k: 0.0,
        });
        router.add_route(ModelRoute {
            model: "c".to_string(),
            priority: 1,
            price_in_per_1k: 0.0,
            price_out_per_1k: 0.0,
        });
        let chain: Vec<_> = router.chain().iter().map(|r| r.model.as_str()).collect();
        assert_eq!(chain, vec!["a", "b", "c"]);
        assert_eq!(router.pick().unwrap().model, "a");
    }

    #[test]
    fn cost_math_books_and_unknown_models_fail() {
        let mut tracker = CostTracker::new();
        tracker.set_price("m1", 1.0, 3.0);
        let cost = tracker.record("m1", 2000, 1000).unwrap();
        assert!((cost - 5.0).abs() < f64::EPSILON);
        assert!((tracker.spend() - 5.0).abs() < f64::EPSILON);
        assert_eq!(
            tracker.record("m9", 1, 1).unwrap_err(),
            RoutingError::UnknownModel("m9".to_string())
        );
    }
}
