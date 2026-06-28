//! Status condition helpers for headmaster-owned resources.
//!
//! The public surface is the `ResourceStatus` trait. Controllers call `update_ready` on a
//! cloned status struct and never touch conditions directly.
//!
//! `set_condition_at` preserves `last_transition_time`: the Kubernetes convention is that
//! the timestamp only advances when `status` actually flips (True → False, etc.). The API
//! server does not enforce this; we carry the existing timestamp forward ourselves.

pub use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

// ── public trait ──────────────────────────────────────────────────────────────

/// Implemented by every status struct managed by headmaster.
///
/// Provides default `is_ready` and `update_ready` implementations backed by the
/// two required field accessors. Controllers clone the status, call `update_ready`
/// in place, then patch via SSA.
pub trait ResourceStatus {
    fn conditions(&self) -> &[Condition];
    fn conditions_mut(&mut self) -> &mut Vec<Condition>;
    fn set_observed_generation(&mut self, _generation: i64) {}

    /// `true` if no conditions have been written yet (first reconcile).
    fn is_new(&self) -> bool {
        self.conditions().is_empty()
    }

    /// `true` if the `Ready` condition is `"True"`.
    fn is_ready(&self) -> bool {
        self.conditions()
            .iter()
            .find(|c| c.type_ == "Ready")
            .map(|c| c.status == "True")
            .unwrap_or(false)
    }

    /// Upsert the `Ready` condition at an explicit timestamp.
    ///
    /// Exposed so tests can supply a deterministic timestamp; the runtime path uses
    /// `update_ready`. `last_transition_time` is preserved when the status value does not change.
    fn update_ready_at(
        &mut self,
        is_ready: bool,
        reason: impl Into<String>,
        message: impl Into<String>,
        generation: i64,
        at: Time,
    ) {
        set_condition_at(
            self.conditions_mut(),
            "Ready",
            is_ready,
            reason.into(),
            message.into(),
            generation,
            at,
        );
        self.set_observed_generation(generation);
    }

    /// Upsert the `Ready` condition using the current wall-clock time.
    // TODO: bool can't express the "Unknown" status required by AGENTS.md; change
    // to a 3-way enum (True/False/Unknown) when a controller needs it.
    fn update_ready(
        &mut self,
        is_ready: bool,
        reason: impl Into<String>,
        message: impl Into<String>,
        generation: i64,
    ) {
        self.update_ready_at(
            is_ready,
            reason,
            message,
            generation,
            Time(k8s_openapi::jiff::Timestamp::now()),
        );
    }
}

// ── private helpers ───────────────────────────────────────────────────────────

fn set_condition_at(
    conditions: &mut Vec<Condition>,
    type_: &str,
    is_ready: bool,
    reason: String,
    message: String,
    generation: i64,
    now: Time,
) {
    let status_str = if is_ready { "True" } else { "False" };
    let new = Condition {
        type_: type_.to_string(),
        status: status_str.to_string(),
        reason,
        message,
        observed_generation: Some(generation),
        last_transition_time: now,
    };
    if let Some(existing) = conditions.iter_mut().find(|c| c.type_ == type_) {
        let ltt = if existing.status == status_str {
            existing.last_transition_time.clone()
        } else {
            new.last_transition_time.clone()
        };
        *existing = new;
        existing.last_transition_time = ltt;
    } else {
        conditions.push(new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use k8s_openapi::jiff::Timestamp;

    // ── helpers ───────────────────────────────────────────────────────────────

    struct TestStatus(Vec<Condition>);
    impl ResourceStatus for TestStatus {
        fn conditions(&self) -> &[Condition] {
            &self.0
        }
        fn conditions_mut(&mut self) -> &mut Vec<Condition> {
            &mut self.0
        }
    }

    fn t(secs: i64) -> Time {
        Time(Timestamp::from_second(secs).unwrap())
    }

    // ── tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn replaces_by_type() {
        let mut s = TestStatus(vec![]);
        s.update_ready_at(true, "AllGood", "msg", 1, t(1000));
        s.update_ready_at(false, "NotReady", "msg", 1, t(2000));
        assert_eq!(s.0.len(), 1);
        assert_eq!(s.0[0].status, "False");
        assert_eq!(s.0[0].reason, "NotReady");
    }

    #[test]
    fn preserves_last_transition_time_when_status_unchanged() {
        let mut s = TestStatus(vec![]);
        s.update_ready_at(true, "AllGood", "msg", 1, t(1000));
        s.update_ready_at(true, "StillGood", "msg", 1, t(2000));
        assert_eq!(s.0[0].last_transition_time, t(1000));
        assert_eq!(s.0[0].reason, "StillGood");
    }

    #[test]
    fn updates_last_transition_time_when_status_changes() {
        let mut s = TestStatus(vec![]);
        s.update_ready_at(true, "AllGood", "msg", 1, t(1000));
        s.update_ready_at(false, "NotReady", "msg", 1, t(2000));
        assert_eq!(s.0[0].last_transition_time, t(2000));
    }

    #[test]
    fn is_new_true_with_no_conditions() {
        assert!(TestStatus(vec![]).is_new());
    }

    #[test]
    fn is_new_false_after_any_condition_written() {
        let mut s = TestStatus(vec![]);
        s.update_ready_at(true, "AllGood", "msg", 1, t(1000));
        assert!(!s.is_new());
    }

    #[test]
    fn is_ready_true_when_ready_condition_is_true() {
        let mut s = TestStatus(vec![]);
        s.update_ready_at(true, "AllGood", "msg", 1, t(1000));
        assert!(s.is_ready());
    }

    #[test]
    fn is_ready_false_when_ready_condition_is_false() {
        let mut s = TestStatus(vec![]);
        s.update_ready_at(false, "NotReady", "msg", 1, t(1000));
        assert!(!s.is_ready());
    }

    #[test]
    fn is_ready_false_with_no_conditions() {
        assert!(!TestStatus(vec![]).is_ready());
    }
}
