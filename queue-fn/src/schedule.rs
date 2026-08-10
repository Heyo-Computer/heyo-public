//! Time-based triggers.
//!
//! Deliberately not cron. A cron parser is either ~400 lines of edge cases or a
//! dependency that drags in uuid, chrono, and a job store — for expressiveness
//! nothing has asked for. `Interval` and `DailyAt` cover the cases that actually
//! come up, and both publish to the same subject an external caller would, so a
//! scheduled invocation is indistinguishable from any other once it is on the
//! bus.
//!
//! **Firing is idempotent by construction.** The invocation id is derived from
//! the function and the time slot rather than minted fresh, so a restart inside
//! the same slot re-publishes an id JetStream has already seen and the dedupe
//! window collapses it. Without that, every deploy would double-fire whatever
//! was due that minute.

use crate::bus::{Bus, EventSource, InvokeEvent, now_ms};
use crate::config::{TriggerSpec, parse_hhmm};
use crate::registry::Registry;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Finer than the minute resolution of `DailyAt`, so a slot cannot be stepped
/// over, and coarse enough that the loop is nearly free.
const TICK: Duration = Duration::from_secs(30);

const SECS_PER_MINUTE: u64 = 60;
const MINUTES_PER_DAY: u64 = 24 * 60;

/// A firing slot, identified so the same slot can be recognised twice.
///
/// Both variants floor to a whole slot rather than using the current time, which
/// is what makes a re-fire within the slot produce the same id.
fn slot_id(function_id: &str, trigger_index: usize, slot: u64) -> String {
    // Hex and dashes only: this becomes both a NATS token and the daemon's
    // operationId, neither of which is escaped.
    format!("sched-{function_id}-{trigger_index:x}-{slot:012x}")
}

/// Which interval slot `now_secs` falls in, for a trigger of `every_secs`.
fn interval_slot(now_secs: u64, every_secs: u64) -> u64 {
    now_secs / every_secs.max(1)
}

/// Whole days since the epoch, so two firings of `09:00` on different days get
/// different slots.
fn day_index(now_secs: u64) -> u64 {
    now_secs / (SECS_PER_MINUTE * MINUTES_PER_DAY)
}

/// Whether a `DailyAt` time is due in the window `(previous_tick, now]`.
///
/// A window rather than an equality check: the ticker is 30s and a slot is a
/// minute, so testing `minute_of_day == target` would fire twice for the same
/// minute. Testing the window fires exactly once, and still catches a slot the
/// loop was late for.
fn daily_due(target_minute: u64, previous_secs: u64, now_secs: u64) -> bool {
    if now_secs <= previous_secs {
        return false;
    }
    let prev_minute_abs = previous_secs / SECS_PER_MINUTE;
    let now_minute_abs = now_secs / SECS_PER_MINUTE;
    // Walk the minutes that elapsed, capped so a long pause (a laptop resuming
    // from sleep, a debugger) replays at most one day rather than thousands of
    // firings.
    let span = (now_minute_abs - prev_minute_abs).min(MINUTES_PER_DAY);
    ((now_minute_abs - span + 1)..=now_minute_abs).any(|m| m % MINUTES_PER_DAY == target_minute)
}

pub async fn run(
    registry: Arc<Registry>,
    bus: Arc<Bus>,
    max_payload_bytes: usize,
    mut shutdown: watch::Receiver<bool>,
) {
    tracing::info!("scheduler starting");
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut previous_secs = now_ms() / 1000;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now_secs = now_ms() / 1000;
                fire_due(&registry, &bus, max_payload_bytes, previous_secs, now_secs).await;
                previous_secs = now_secs;
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("scheduler shutting down");
                    return;
                }
            }
        }
    }
}

async fn fire_due(
    registry: &Arc<Registry>,
    bus: &Arc<Bus>,
    max_payload_bytes: usize,
    previous_secs: u64,
    now_secs: u64,
) {
    for f in registry.functions().values() {
        // A paused function keeps its schedule but stops accumulating from it;
        // otherwise a pause over a weekend would produce a stampede on resume.
        if f.is_paused() {
            continue;
        }

        for (i, trigger) in f.spec.triggers.iter().enumerate() {
            let slot = match trigger {
                TriggerSpec::Interval { every_secs, .. } => {
                    let current = interval_slot(now_secs, *every_secs);
                    if current == interval_slot(previous_secs, *every_secs) {
                        continue; // same slot as last tick
                    }
                    current
                }
                TriggerSpec::DailyAt { times, .. } => {
                    let due = times
                        .iter()
                        .filter_map(|t| parse_hhmm(t))
                        .find(|&m| daily_due(m as u64, previous_secs, now_secs));
                    match due {
                        // Day plus minute, so the same clock time tomorrow is a
                        // different slot.
                        Some(minute) => day_index(now_secs) * MINUTES_PER_DAY + minute as u64,
                        None => continue,
                    }
                }
            };

            let mut event = InvokeEvent::new(
                &f.spec.id,
                trigger.payload().cloned(),
                EventSource::Schedule,
            );
            // Slot-derived, not random: a restart inside the same slot
            // re-publishes an id JetStream already deduped.
            event.invocation_id = slot_id(&f.spec.id, i, slot);

            let limit = f.spec.exec.max_payload_bytes.min(max_payload_bytes);
            match bus.publish(&event, limit).await {
                Ok(()) => {
                    tracing::info!(
                        function = %f.spec.id,
                        trigger = i,
                        invocation = %event.invocation_id,
                        "scheduled trigger fired",
                    );
                    f.note_enqueued();
                    f.scale_signal.notify_one();
                }
                Err(e) => {
                    tracing::warn!(
                        function = %f.spec.id,
                        trigger = i,
                        error = %e,
                        "could not publish a scheduled trigger",
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_slots_advance_once_per_period() {
        assert_eq!(interval_slot(0, 60), 0);
        assert_eq!(interval_slot(59, 60), 0);
        assert_eq!(interval_slot(60, 60), 1);
        assert_eq!(interval_slot(119, 60), 1);
        assert_eq!(interval_slot(120, 60), 2);
    }

    /// A zero period would divide by zero. `validate` rejects it, but the
    /// arithmetic must not panic if a spec ever reaches here unvalidated.
    #[test]
    fn a_zero_interval_does_not_divide_by_zero() {
        assert_eq!(interval_slot(100, 0), 100);
    }

    /// Regression: an interval trigger fired on every 30s tick because the tick
    /// itself was treated as the firing signal. Comparing the *slot* against the
    /// previous tick's makes a 60s trigger fire once a minute, not twice.
    #[test]
    fn an_interval_fires_once_per_slot_not_once_per_tick() {
        let every = 60;
        let ticks = [0u64, 30, 60, 90, 120];
        let fired: Vec<_> = ticks
            .windows(2)
            .filter(|w| interval_slot(w[1], every) != interval_slot(w[0], every))
            .map(|w| w[1])
            .collect();
        assert_eq!(fired, [60, 120], "one firing per minute, not per tick");
    }

    /// Regression: `DailyAt` compared `minute_of_day == target`, which is true
    /// for both 30s ticks inside that minute — so every daily trigger fired
    /// twice. Only the tick that *crosses into* the minute should fire.
    #[test]
    fn a_daily_trigger_fires_once_even_though_two_ticks_fall_in_its_minute() {
        // 09:00 UTC == minute 540.
        let target = 540;
        let nine_am = 540 * 60;
        let ticks = [nine_am - 30, nine_am, nine_am + 30, nine_am + 60];

        let fired: Vec<_> = ticks
            .windows(2)
            .filter(|w| daily_due(target, w[0], w[1]))
            .map(|w| w[1])
            .collect();
        assert_eq!(fired.len(), 1, "fired at {fired:?}, expected exactly once");
        assert_eq!(fired[0], nine_am);
    }

    #[test]
    fn a_daily_trigger_does_not_fire_outside_its_minute() {
        let target = 540;
        let noon = 12 * 60 * 60;
        assert!(!daily_due(target, noon, noon + 30));
    }

    /// A tick that arrives late — a paused process, a slow reconcile — must
    /// still fire the slot it stepped over rather than skipping the day.
    #[test]
    fn a_late_tick_still_fires_the_slot_it_stepped_over() {
        let target = 540;
        let nine_am = 540 * 60;
        assert!(
            daily_due(target, nine_am - 300, nine_am + 300),
            "a ten-minute gap spanning 09:00 must still fire it",
        );
    }

    /// Regression: a machine resuming from sleep after a week replayed every
    /// missed minute, publishing thousands of events at once. The lookback is
    /// capped at a day so a long pause costs at most one firing per trigger.
    #[test]
    fn a_very_long_gap_does_not_stampede() {
        let target = 540;
        let start = 540 * 60;
        let a_week_later = start + 7 * 24 * 3600;
        // Still fires once — but only once, not once per missed day.
        assert!(daily_due(target, start, a_week_later));
    }

    #[test]
    fn time_going_backwards_fires_nothing() {
        let target = 540;
        let nine_am = 540 * 60;
        assert!(
            !daily_due(target, nine_am + 100, nine_am),
            "a backwards clock step must not fire, and must not underflow",
        );
        assert!(!daily_due(target, nine_am, nine_am));
    }

    /// Regression: the slot id was a fresh random id, so a restart inside a
    /// trigger's minute fired it a second time. Deriving it from the slot means
    /// the republish is deduped by JetStream instead.
    #[test]
    fn a_slot_id_is_stable_within_a_slot_and_distinct_across_them() {
        let a = slot_id("demo", 0, 100);
        let b = slot_id("demo", 0, 100);
        assert_eq!(a, b, "the same slot must produce the same id");

        assert_ne!(a, slot_id("demo", 0, 101), "next slot");
        assert_ne!(a, slot_id("demo", 1, 100), "a different trigger");
        assert_ne!(a, slot_id("other", 0, 100), "a different function");
    }

    #[test]
    fn slot_ids_are_valid_operation_and_subject_tokens() {
        let id = slot_id("my-fn", 3, 0xdead_beef);
        assert!(crate::vm::valid_operation_id(&id), "got {id}");
        assert!(!id.contains('.'), "a dot would corrupt the NATS subject");
    }

    #[test]
    fn the_same_clock_time_on_different_days_is_a_different_slot() {
        let nine_am = 540 * 60;
        let tomorrow = nine_am + 24 * 3600;
        let a = day_index(nine_am) * MINUTES_PER_DAY + 540;
        let b = day_index(tomorrow) * MINUTES_PER_DAY + 540;
        assert_ne!(a, b, "or every day's firing would be deduped away as a repeat");
    }

    /// Midnight is minute 0, so the window arithmetic has to wrap rather than
    /// treating it as "before the start of the day" and never firing.
    #[test]
    fn a_midnight_trigger_fires() {
        let midnight = 24 * 3600; // the start of the next day
        assert!(daily_due(0, midnight - 30, midnight));
        assert!(!daily_due(0, midnight, midnight + 30), "and only once");
    }

    /// The tick must be finer than the resolution of the finest trigger, or a
    /// slot can be stepped over entirely between two ticks.
    #[test]
    fn the_tick_is_finer_than_the_slot_resolution() {
        assert!(
            TICK.as_secs() < SECS_PER_MINUTE,
            "a tick at or above a minute could skip a DailyAt slot",
        );
        assert!(
            TICK.as_secs() <= crate::config::MIN_INTERVAL_SECS,
            "a tick coarser than the minimum interval could skip an Interval slot",
        );
    }
}
