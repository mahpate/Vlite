//! Work budget, deadline and cancellation accounting of one read.

use super::limits::{CHECK_INTERVAL, GraphLimits};
use std::sync::atomic::{AtomicBool, Ordering};

/// Borrowed per-call execution context, kept apart from the owned
/// `'static` query and limits so those can be stored, compared and reused.
#[derive(Clone, Copy)]
pub struct ExecutionContext<'a> {
    /// The commit sequence the caller read the graph at; echoed in results.
    pub view_sequence: u64,
    /// Monotonic milliseconds. Consulted once before work starts, every
    /// `CHECK_INTERVAL` budget units and once at the end for `elapsed_ms`.
    pub clock: &'a dyn Fn() -> u64,
    /// Set by another thread to stop the read at the next check.
    pub cancel: Option<&'a AtomicBool>,
}

impl<'a> ExecutionContext<'a> {
    pub fn new(view_sequence: u64, clock: &'a dyn Fn() -> u64) -> Self {
        Self {
            view_sequence,
            clock,
            cancel: None,
        }
    }
}

/// Why a read stopped early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stop {
    EdgeLimit,
    Deadline,
    Cancelled,
}

pub(super) struct Meter<'a> {
    context: &'a ExecutionContext<'a>,
    started: u64,
    deadline_ms: u64,
    budget: u64,
    spent: u64,
}

impl<'a> Meter<'a> {
    /// Reads the clock and checks cancellation and the deadline once before
    /// any work.
    pub(super) fn start(
        context: &'a ExecutionContext<'a>,
        limits: &GraphLimits,
    ) -> (Self, Result<(), Stop>) {
        let meter = Self {
            context,
            started: (context.clock)(),
            deadline_ms: limits.deadline_ms,
            budget: limits.max_edges_examined,
            spent: 0,
        };
        let check = meter
            .check_cancel()
            .and_then(|()| meter.check_deadline(meter.started));
        (meter, check)
    }

    fn check_cancel(&self) -> Result<(), Stop> {
        match self.context.cancel {
            Some(flag) if flag.load(Ordering::Relaxed) => Err(Stop::Cancelled),
            _ => Ok(()),
        }
    }

    fn check_deadline(&self, now: u64) -> Result<(), Stop> {
        if now.saturating_sub(self.started) >= self.deadline_ms {
            Err(Stop::Deadline)
        } else {
            Ok(())
        }
    }

    /// Spends one budget unit. Fails without spending when the budget is
    /// exhausted, or when a periodic check finds the deadline passed or the
    /// read cancelled.
    pub(super) fn charge(&mut self) -> Result<(), Stop> {
        if self.spent >= self.budget {
            return Err(Stop::EdgeLimit);
        }
        if self.spent > 0 && self.spent.is_multiple_of(CHECK_INTERVAL) {
            let now = (self.context.clock)();
            self.check_cancel()?;
            self.check_deadline(now)?;
        }
        self.spent += 1;
        Ok(())
    }

    pub(super) fn spent(&self) -> u64 {
        self.spent
    }

    /// Reads the clock a final time.
    pub(super) fn elapsed_ms(&self) -> u64 {
        (self.context.clock)().saturating_sub(self.started)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn limits(budget: u64, deadline_ms: u64) -> GraphLimits {
        GraphLimits {
            max_edges_examined: budget,
            deadline_ms,
            ..GraphLimits::DEFAULT
        }
    }

    fn spend(meter: &mut Meter<'_>) -> (u64, Stop) {
        loop {
            if let Err(stop) = meter.charge() {
                return (meter.spent(), stop);
            }
        }
    }

    #[test]
    fn the_budget_is_exact() {
        let clock = || 7;
        let context = ExecutionContext::new(0, &clock);
        for budget in [1, 63, 64, 65, 1_000] {
            let (mut meter, begin) = Meter::start(&context, &limits(budget, 1));
            assert_eq!(begin, Ok(()));
            assert_eq!(spend(&mut meter), (budget, Stop::EdgeLimit));
            assert_eq!(meter.elapsed_ms(), 0);
        }
    }

    #[test]
    fn the_deadline_is_checked_before_work_and_every_64_units() {
        let now = Cell::new(0u64);
        let reads = Cell::new(0u64);
        let clock = || {
            reads.set(reads.get() + 1);
            now.set(now.get() + 10);
            now.get()
        };
        let context = ExecutionContext::new(0, &clock);
        // Start reads 10; checks at 64, 128, 192 read 20, 30, 40.
        let (mut meter, begin) = Meter::start(&context, &limits(100_000, 25));
        assert_eq!(begin, Ok(()));
        assert_eq!(spend(&mut meter), (192, Stop::Deadline));
        assert_eq!(reads.get(), 4);
        assert_eq!(meter.elapsed_ms(), 40);
        // A clock already past the deadline at the first check stops at 64.
        let (mut meter, _) = Meter::start(&context, &limits(100_000, 1));
        assert_eq!(spend(&mut meter), (64, Stop::Deadline));
        // A clock that goes backwards never underflows.
        let backwards = Cell::new(1_000u64);
        let clock = || {
            backwards.set(backwards.get().saturating_sub(100));
            backwards.get()
        };
        let context = ExecutionContext::new(0, &clock);
        let (mut meter, begin) = Meter::start(&context, &limits(200, 1));
        assert_eq!(begin, Ok(()));
        assert_eq!(spend(&mut meter), (200, Stop::EdgeLimit));
        assert_eq!(meter.elapsed_ms(), 0);
    }

    #[test]
    fn cancellation_is_checked_before_work_and_every_64_units() {
        let clock = || 0;
        let flag = AtomicBool::new(true);
        let context = ExecutionContext {
            cancel: Some(&flag),
            ..ExecutionContext::new(0, &clock)
        };
        let (mut meter, begin) = Meter::start(&context, &limits(1_000, 1_000));
        assert_eq!(begin, Err(Stop::Cancelled));
        assert_eq!(spend(&mut meter), (64, Stop::Cancelled));

        let flag = AtomicBool::new(false);
        let reads = Cell::new(0u64);
        let clock = || {
            reads.set(reads.get() + 1);
            if reads.get() == 3 {
                flag.store(true, Ordering::Relaxed);
            }
            0
        };
        let context = ExecutionContext {
            cancel: Some(&flag),
            ..ExecutionContext::new(0, &clock)
        };
        let (mut meter, begin) = Meter::start(&context, &limits(1_000, 1_000));
        assert_eq!(begin, Ok(()));
        assert_eq!(spend(&mut meter), (128, Stop::Cancelled));
    }
}
