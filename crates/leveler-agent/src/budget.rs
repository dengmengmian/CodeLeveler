//! Atomic in-flight budget reservations for shared model budgets.
//!
//! The durable ledger records one row per *settled* model attempt. Two
//! concurrent admissions that both read that ledger before either attempt
//! settles can therefore both pass the same balance check, and the total
//! spend can exceed the configured cap by up to `N × per-request estimate`.
//!
//! This module closes that window for one runtime process:
//!
//! - [`BudgetReservations::admit`] holds the scope lock while it reads the
//!   settled progress and adds the request's worst case, so two admissions
//!   for the same budget scope cannot both ignore each other.
//! - A [`BudgetReservation`] is released only after the attempt's request row
//!   is durable. An admission that runs while a settlement is being written
//!   still sees the reservation, so the invariant
//!   `settled rows + in-flight reservations ≥ actual spend` holds.
//! - Dropping a reservation without settling releases it (a cancelled or
//!   aborted attempt spent what the provider reported; if the row could not be
//!   written the caller keeps the hold instead).
//!
//! The reservation is per process. A second process that shares the same
//! session/budget scope (not a supported topology for one active goal) has its
//! own ledger; the durable settled rows remain the cross-process source of
//! truth, and this module never claims a hard ledger limit it cannot enforce.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use leveler_lifecycle::ProgressLedger;
use leveler_model::{ModelPricing, TokenUsage};
use tokio::sync::Mutex;

use crate::executor::{AgentError, StepLimits};

#[derive(Clone, Copy, Default)]
struct InFlight {
    tokens: u64,
    cost_micros: u64,
}

/// Per-scope in-flight reservations shared by every executor in one runtime.
#[derive(Clone, Default)]
pub(crate) struct BudgetReservations {
    inner: Arc<Mutex<HashMap<String, InFlight>>>,
}

impl BudgetReservations {
    /// Reserve this request's worst case for `key`.
    ///
    /// `settled` is evaluated while the scope lock is held. That ordering is
    /// the point: a settlement writes its durable row before it releases its
    /// reservation, so a reader inside the lock either sees the row or the
    /// reservation, never neither.
    pub(crate) async fn admit<F>(
        &self,
        key: &str,
        settled: F,
        limits: StepLimits,
        input: u64,
        output: u64,
        pricing: Option<&ModelPricing>,
    ) -> Result<Option<BudgetReservation>, AgentError>
    where
        F: Future<Output = Result<Option<ProgressLedger>, AgentError>> + Send,
    {
        let mut held = self.inner.lock().await;
        let Some(settled) = settled.await? else {
            return Ok(None);
        };
        let reserved = held.get(key).copied().unwrap_or_default();
        let mut combined = settled;
        combined.cumulative_model_tokens = combined
            .cumulative_model_tokens
            .saturating_add(reserved.tokens);
        combined.cumulative_cost_usd_micros = combined
            .cumulative_cost_usd_micros
            .saturating_add(reserved.cost_micros);
        crate::executor::model_request_budget_check(limits, &combined, input, output, pricing)?;
        let cost_micros = requested_cost(limits, input, output, pricing)?.unwrap_or(0);
        let tokens = input.saturating_add(output);
        let entry = held.entry(key.to_string()).or_default();
        entry.tokens = entry.tokens.saturating_add(tokens);
        entry.cost_micros = entry.cost_micros.saturating_add(cost_micros);
        Ok(Some(BudgetReservation {
            inner: Arc::clone(&self.inner),
            key: key.to_string(),
            tokens,
            cost_micros,
            released: false,
        }))
    }
}

/// The worst-case auditable cost of one request under an active cost cap.
fn requested_cost(
    limits: StepLimits,
    input: u64,
    output: u64,
    pricing: Option<&ModelPricing>,
) -> Result<Option<u64>, AgentError> {
    if limits.max_cost_usd_micros.is_none() {
        return Ok(None);
    }
    let usage = TokenUsage {
        input_tokens: input,
        output_tokens: output,
        ..Default::default()
    };
    pricing
        .and_then(|pricing| pricing.cost_for_usage(&usage))
        .map(Some)
        .ok_or_else(|| {
            AgentError::InvalidBudget("request pricing is unknown under active cost cap".into())
        })
}

/// One admitted request's hold on a budget scope.
pub(crate) struct BudgetReservation {
    inner: Arc<Mutex<HashMap<String, InFlight>>>,
    key: String,
    tokens: u64,
    cost_micros: u64,
    released: bool,
}

impl BudgetReservation {
    /// Settle the request: its durable row now carries the actual spend.
    pub(crate) async fn release(mut self) {
        self.released = true;
        self.drop_hold().await;
    }

    /// Keep the hold for the rest of the process.
    ///
    /// Used when the provider spend happened but its durable row could not be
    /// written. Releasing would let the next admission forget real spend.
    pub(crate) fn hold(mut self) {
        self.released = true;
    }

    async fn drop_hold(&mut self) {
        let mut held = self.inner.lock().await;
        let Some(entry) = held.get_mut(&self.key) else {
            return;
        };
        entry.tokens = entry.tokens.saturating_sub(self.tokens);
        entry.cost_micros = entry.cost_micros.saturating_sub(self.cost_micros);
        if entry.tokens == 0 && entry.cost_micros == 0 {
            held.remove(&self.key);
        }
    }
}

impl Drop for BudgetReservation {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let key = std::mem::take(&mut self.key);
        let tokens = self.tokens;
        let cost_micros = self.cost_micros;
        // Drop cannot await. A reservation dropped with no runtime is kept:
        // over-counting is safe for a cap, under-counting is not.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut held = inner.lock().await;
                let Some(entry) = held.get_mut(&key) else {
                    return;
                };
                entry.tokens = entry.tokens.saturating_sub(tokens);
                entry.cost_micros = entry.cost_micros.saturating_sub(cost_micros);
                if entry.tokens == 0 && entry.cost_micros == 0 {
                    held.remove(&key);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(tokens: u64) -> StepLimits {
        StepLimits {
            max_model_tokens: Some(tokens),
            ..StepLimits::default()
        }
    }

    fn settled(tokens: u64) -> impl Future<Output = Result<Option<ProgressLedger>, AgentError>> {
        std::future::ready(Ok(Some(ProgressLedger {
            cumulative_model_tokens: tokens,
            ..ProgressLedger::default()
        })))
    }

    /// Two requests that each fit the settled balance, but do not fit
    /// together, cannot both be admitted. This is the defect the reservation
    /// exists for: settled-only admission let both through.
    #[tokio::test]
    async fn a_second_concurrent_request_cannot_ignore_the_first_reservation() {
        let reservations = BudgetReservations::default();
        let first = reservations
            .admit("session:goal", settled(0), limits(100), 60, 10, None)
            .await
            .unwrap()
            .expect("first admission must reserve");
        let second = reservations
            .admit("session:goal", settled(0), limits(100), 60, 10, None)
            .await;
        assert!(
            matches!(second, Err(AgentError::BudgetExhausted(_))),
            "the second request must see the first request's in-flight 70 tokens"
        );
        first.release().await;
        let third = reservations
            .admit("session:goal", settled(0), limits(100), 60, 10, None)
            .await;
        assert!(third.is_ok(), "releasing the hold reopens the balance");
    }

    /// A different budget scope does not share the reservation.
    #[tokio::test]
    async fn reservations_are_per_budget_scope() {
        let reservations = BudgetReservations::default();
        let first = reservations
            .admit("a", settled(0), limits(100), 60, 10, None)
            .await
            .unwrap()
            .unwrap();
        let other = reservations
            .admit("b", settled(0), limits(100), 60, 10, None)
            .await;
        assert!(other.is_ok());
        first.release().await;
    }

    /// A settled row plus an in-flight request both count against the cap.
    #[tokio::test]
    async fn settled_spend_and_in_flight_reservations_are_added() {
        let reservations = BudgetReservations::default();
        let first = reservations
            .admit("s", settled(50), limits(100), 30, 10, None)
            .await
            .unwrap()
            .unwrap();
        let second = reservations
            .admit("s", settled(50), limits(100), 30, 10, None)
            .await;
        assert!(second.is_err(), "50 settled + 40 in flight + 40 > 100");
        first.release().await;
    }
}
