#![cfg_attr(not(test), allow(dead_code))]
#![cfg(test)]

//! An offline interleaving model, not an executing coordinator or fence.

use sha2::{Digest, Sha256};

use super::authority::{HeadRef, Observed, Phase, Record};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Error {
    Acknowledgement,
    Stale,
    Fence,
    Uncertain,
    AlreadyDispatched,
    Cancelled,
    AlreadyTransferred,
    ActuatorUnproven,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    AwaitingReport,
    ReportReady,
    Acknowledged,
    TransferPending,
    Reconciling,
    Indeterminate,
    TransferPrepared,
    Conflict,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RetainedReport {
    generation: u64,
    authority: Observed,
    digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Acknowledgement {
    report_digest: String,
    report_generation: u64,
    authority: Observed,
    actor: String,
}

#[derive(Clone)]
struct Pending {
    expected: Observed,
    candidate: Record,
    candidate_bytes: Vec<u8>,
    acknowledgement: Acknowledgement,
    attempt: String,
    may_have_dispatched: bool,
}

struct ActuatorStopped {
    attempt: String,
}

trait AuthorityPort {
    fn read(&self) -> Result<Observed, Error>;
    fn final_fence_check(&self) -> Result<(), Error>;
    fn cas(&mut self, expected_etag: &str, candidate: &Record);
}

#[derive(Clone)]
struct FakePort {
    current: Observed,
    readable: bool,
    writer_stopped: bool,
    apply: bool,
    dispatches: u64,
    competitor_at_cas: Option<Observed>,
}

impl AuthorityPort for FakePort {
    fn read(&self) -> Result<Observed, Error> {
        if self.readable {
            Ok(self.current.clone())
        } else {
            Err(Error::Uncertain)
        }
    }

    fn final_fence_check(&self) -> Result<(), Error> {
        if self.writer_stopped {
            Ok(())
        } else {
            Err(Error::Fence)
        }
    }

    fn cas(&mut self, expected_etag: &str, candidate: &Record) {
        self.dispatches += 1;
        if let Some(competitor) = self.competitor_at_cas.take() {
            self.current = competitor;
        }
        if self.apply && self.current.etag == expected_etag {
            self.current = Observed {
                record: candidate.clone(),
                etag: format!("etag-{}", self.dispatches),
            };
        }
    }
}

impl FakePort {
    fn advance_head(&mut self) {
        self.current.record.head.as_mut().unwrap().sequence += 1;
        self.current.etag.push_str("-advanced");
    }
}

#[derive(Clone)]
struct Model {
    port: FakePort,
    attempt: String,
    report: Option<RetainedReport>,
    report_generation: u64,
    acknowledgement: Option<Acknowledgement>,
    pending: Option<Pending>,
    state: State,
    terminal: Option<Observed>,
}

impl Model {
    fn ready() -> Self {
        let record = Record {
            schema: "vlite.writer-authority/1".into(),
            installation_id: "installation".into(),
            scope_id: "scope".into(),
            lineage: "0123456789abcdef".into(),
            writer_id: Some("a".repeat(32)),
            epoch: 1,
            revision: "b".repeat(32),
            phase: Phase::Serving,
            base: None,
            head: Some(HeadRef {
                epoch: 1,
                set_id: "20261118T000000Z-0123456789abcdef".into(),
                manifest_sha256: "c".repeat(64),
                index_sha256: Some("d".repeat(64)),
                sequence: 10,
                chain_sha256: "e".repeat(64),
            }),
            claimed_unix_ms: Some(42),
        };
        record.validate().unwrap();
        let mut model = Self {
            port: FakePort {
                current: Observed {
                    record,
                    etag: "old-etag".into(),
                },
                readable: true,
                writer_stopped: true,
                apply: true,
                dispatches: 0,
                competitor_at_cas: None,
            },
            attempt: "attempt-a".into(),
            report: None,
            report_generation: 0,
            acknowledgement: None,
            pending: None,
            state: State::ReportReady,
            terminal: None,
        };
        model.publish_report("initial diagnostic").unwrap();
        model
    }

    fn dispatched() -> Self {
        let mut model = Self::ready();
        model.acknowledge().unwrap();
        model.prepare().unwrap();
        model.dispatch().unwrap();
        model
    }

    fn ensure_editable(&self) -> Result<(), Error> {
        if self.state == State::Cancelled {
            return Err(Error::Cancelled);
        }
        if self.terminal.is_some() {
            return Err(Error::AlreadyTransferred);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.may_have_dispatched)
        {
            return Err(Error::Uncertain);
        }
        Ok(())
    }

    fn publish_report(&mut self, diagnostic: &str) -> Result<(), Error> {
        self.ensure_editable()?;
        let authority = self.port.read()?;
        authority.record.validate().map_err(|_| Error::Invalid)?;
        let generation = self
            .report_generation
            .checked_add(1)
            .ok_or(Error::Invalid)?;
        let bytes =
            serde_json::to_vec(&(&authority.record, &authority.etag, diagnostic, generation))
                .map_err(|_| Error::Invalid)?;
        self.report = Some(RetainedReport {
            generation,
            authority,
            digest: format!("{:x}", Sha256::digest(bytes)),
        });
        self.report_generation = generation;
        self.acknowledgement = None;
        self.pending = None;
        self.state = State::ReportReady;
        Ok(())
    }

    fn acknowledge(&mut self) -> Result<(), Error> {
        self.ensure_editable()?;
        let report = self.report.as_ref().ok_or(Error::Acknowledgement)?;
        self.acknowledgement = Some(Acknowledgement {
            report_digest: report.digest.clone(),
            report_generation: report.generation,
            authority: report.authority.clone(),
            actor: "operator".into(),
        });
        self.state = State::Acknowledged;
        Ok(())
    }

    fn invalidate(&mut self) {
        self.report = None;
        self.acknowledgement = None;
        self.pending = None;
        self.state = State::AwaitingReport;
    }

    fn check_ack_and_authority(&self) -> Result<(), Error> {
        let report = self.report.as_ref().ok_or(Error::Acknowledgement)?;
        let ack = self
            .acknowledgement
            .as_ref()
            .ok_or(Error::Acknowledgement)?;
        if ack.report_digest != report.digest
            || ack.report_generation != report.generation
            || ack.authority != report.authority
        {
            return Err(Error::Acknowledgement);
        }
        if self.port.read()? != report.authority {
            return Err(Error::Stale);
        }
        Ok(())
    }

    fn prepare(&mut self) -> Result<(), Error> {
        self.ensure_editable()?;
        let ack = self.acknowledgement.clone().ok_or(Error::Acknowledgement)?;
        if let Err(error) = self.check_ack_and_authority() {
            self.invalidate();
            return Err(error);
        }
        let expected = ack.authority.clone();
        let mut candidate = expected.record.clone();
        candidate.epoch = candidate.epoch.checked_add(1).ok_or(Error::Invalid)?;
        candidate.base = candidate.head.take().or(candidate.base);
        candidate.writer_id = None;
        candidate.claimed_unix_ms = None;
        candidate.phase = Phase::Recovering;
        let identity = serde_json::to_vec(&("model-operation", &self.attempt, &ack.report_digest))
            .map_err(|_| Error::Invalid)?;
        candidate.revision = format!("{:x}", Sha256::digest(identity))[..32].into();
        candidate.validate().map_err(|_| Error::Invalid)?;
        let candidate_bytes = serde_json::to_vec(&candidate).map_err(|_| Error::Invalid)?;
        self.pending = Some(Pending {
            expected,
            candidate,
            candidate_bytes,
            acknowledgement: ack,
            attempt: self.attempt.clone(),
            may_have_dispatched: false,
        });
        self.state = State::TransferPending;
        Ok(())
    }

    fn dispatch(&mut self) -> Result<(), Error> {
        if self.state == State::Cancelled {
            return Err(Error::Cancelled);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.may_have_dispatched)
        {
            return Err(Error::AlreadyDispatched);
        }
        let pending = self.pending.as_ref().ok_or(Error::Acknowledgement)?;
        if pending.attempt != self.attempt {
            return Err(Error::ActuatorUnproven);
        }
        if self.acknowledgement.as_ref() != Some(&pending.acknowledgement) {
            return Err(Error::Acknowledgement);
        }
        // Every check is repeated after candidate persistence and before CAS.
        if let Err(error) = self
            .check_ack_and_authority()
            .and_then(|_| self.port.final_fence_check())
        {
            self.invalidate();
            return Err(error);
        }
        let pending = self.pending.as_mut().ok_or(Error::Acknowledgement)?;
        // This is the durable uncertainty point, before the fake port is called.
        pending.may_have_dispatched = true;
        self.state = State::Reconciling;
        self.port.cas(&pending.expected.etag, &pending.candidate);
        Ok(())
    }

    fn restart(&self) -> Self {
        // All fields represent retained journal or external port state.
        self.clone()
    }

    fn reconcile(&mut self) -> Result<State, Error> {
        if self.terminal.is_some() {
            return Ok(State::TransferPrepared);
        }
        let pending = self.pending.as_ref().ok_or(Error::Acknowledgement)?;
        if !pending.may_have_dispatched {
            return Err(Error::Uncertain);
        }
        self.state = match self.port.read() {
            Err(Error::Uncertain) => State::Indeterminate,
            Err(error) => return Err(error),
            Ok(observed) => {
                let bytes = serde_json::to_vec(&observed.record).map_err(|_| Error::Invalid)?;
                if bytes == pending.candidate_bytes {
                    self.terminal = Some(observed);
                    State::TransferPrepared
                } else if observed == pending.expected {
                    State::Indeterminate
                } else {
                    State::Conflict
                }
            }
        };
        Ok(self.state)
    }

    fn cancel(&mut self) -> Result<(), Error> {
        self.ensure_editable()?;
        self.invalidate();
        self.state = State::Cancelled;
        Ok(())
    }

    fn take_over(&mut self, attempt: &str, stopped: Option<ActuatorStopped>) -> Result<(), Error> {
        if attempt != self.attempt && !stopped.is_some_and(|proof| proof.attempt == self.attempt) {
            return Err(Error::ActuatorUnproven);
        }
        if attempt != self.attempt {
            self.attempt = attempt.into();
            if self
                .pending
                .as_ref()
                .is_none_or(|pending| !pending.may_have_dispatched)
            {
                self.invalidate();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_changed_report_clears_acknowledgement() {
        let mut model = Model::ready();
        model.acknowledge().unwrap();
        let original = model.report.clone();
        model.publish_report("changed diagnostic").unwrap();
        assert_ne!(model.report, original);
        assert!(model.acknowledgement.is_none());
        assert_eq!(model.prepare(), Err(Error::Acknowledgement));
        assert_eq!(model.port.dispatches, 0);
    }

    #[test]
    fn failover_head_advance_requires_new_report_and_ack() {
        let mut model = Model::ready();
        model.acknowledge().unwrap();
        model.port.advance_head();
        assert_eq!(model.prepare(), Err(Error::Stale));
        assert!(model.acknowledgement.is_none());
        model.publish_report("new head").unwrap();
        assert_eq!(model.prepare(), Err(Error::Acknowledgement));
        model.acknowledge().unwrap();
        model.prepare().unwrap();
        model.dispatch().unwrap();
        assert_eq!(model.port.dispatches, 1);
    }

    #[test]
    fn failover_final_recheck_detects_reactivation_and_sends_no_transfer() {
        let mut model = Model::ready();
        model.acknowledge().unwrap();
        model.prepare().unwrap();
        model.port.writer_stopped = false;
        assert_eq!(model.dispatch(), Err(Error::Fence));
        assert_eq!(model.port.dispatches, 0);
        assert!(model.acknowledgement.is_none());
        model.port.writer_stopped = true;
        assert_eq!(model.acknowledge(), Err(Error::Acknowledgement));
        assert_eq!(model.prepare(), Err(Error::Acknowledgement));
        model.publish_report("fresh stop observation").unwrap();
        model.acknowledge().unwrap();
        model.prepare().unwrap();
        model.dispatch().unwrap();
        assert_eq!(model.port.dispatches, 1);
    }

    #[test]
    fn failover_crash_after_cas_before_settle_recovers_exact_candidate() {
        let mut model = Model::ready();
        model.acknowledge().unwrap();
        model.prepare().unwrap();
        let candidate = model.pending.as_ref().unwrap().candidate.clone();
        model.dispatch().unwrap();
        assert_eq!(model.state, State::Reconciling);
        let mut restarted = model.restart();
        assert_eq!(restarted.reconcile(), Ok(State::TransferPrepared));
        assert_eq!(restarted.terminal, Some(restarted.port.current.clone()));
        assert_eq!(restarted.terminal.as_ref().unwrap().record, candidate);
        assert_eq!(restarted.port.dispatches, 1);
    }

    #[test]
    fn failover_unreadable_authority_stays_indeterminate() {
        let mut model = Model::dispatched();
        model.port.readable = false;
        for _ in 0..2 {
            assert_eq!(model.reconcile(), Ok(State::Indeterminate));
            assert_eq!(model.dispatch(), Err(Error::AlreadyDispatched));
        }
        assert!(model.terminal.is_none());
        assert_eq!(model.port.dispatches, 1);
        model.port.readable = true;
        assert_eq!(model.reconcile(), Ok(State::TransferPrepared));
    }

    #[test]
    fn failover_unchanged_readback_does_not_authorize_redispatch() {
        let mut model = Model::ready();
        model.acknowledge().unwrap();
        model.prepare().unwrap();
        model.port.apply = false;
        model.dispatch().unwrap();
        assert_eq!(model.reconcile(), Ok(State::Indeterminate));
        assert_eq!(model.prepare(), Err(Error::Uncertain));
        assert_eq!(model.dispatch(), Err(Error::AlreadyDispatched));
        assert_eq!(model.cancel(), Err(Error::Uncertain));
        assert!(model.pending.is_some());
        assert_eq!(model.port.dispatches, 1);
    }

    #[test]
    fn failover_competing_successor_is_not_attributed_to_this_operation() {
        let mut model = Model::ready();
        model.acknowledge().unwrap();
        model.prepare().unwrap();
        let mut competitor = model.pending.as_ref().unwrap().candidate.clone();
        competitor.revision = "f".repeat(32);
        model.port.competitor_at_cas = Some(Observed {
            record: competitor,
            etag: "competing-etag".into(),
        });
        model.dispatch().unwrap();
        assert_eq!(model.reconcile(), Ok(State::Conflict));
        assert!(model.terminal.is_none());
        assert_eq!(model.port.dispatches, 1);
    }

    #[test]
    fn failover_cancel_racing_transfer_cannot_undo_epoch() {
        let mut before = Model::ready();
        before.acknowledge().unwrap();
        before.prepare().unwrap();
        before.cancel().unwrap();
        assert_eq!(before.dispatch(), Err(Error::Cancelled));
        assert_eq!(before.port.current.record.epoch, 1);
        assert_eq!(before.port.dispatches, 0);
        let mut after = Model::dispatched();
        assert_eq!(after.cancel(), Err(Error::Uncertain));
        assert_eq!(after.reconcile(), Ok(State::TransferPrepared));
        assert_eq!(after.port.current.record.epoch, 2);
        assert_eq!(after.cancel(), Err(Error::AlreadyTransferred));
        assert_eq!(after.port.current.record.epoch, 2);
    }

    #[test]
    fn failover_foreign_attempt_requires_actuator_stopped_proof() {
        let mut model = Model::dispatched();
        assert_eq!(
            model.take_over("attempt-b", None),
            Err(Error::ActuatorUnproven)
        );
        assert_eq!(model.attempt, "attempt-a");
        let wrong = ActuatorStopped {
            attempt: "unrelated".into(),
        };
        assert_eq!(
            model.take_over("attempt-b", Some(wrong)),
            Err(Error::ActuatorUnproven)
        );
        let stopped = ActuatorStopped {
            attempt: "attempt-a".into(),
        };
        model.take_over("attempt-b", Some(stopped)).unwrap();
        assert_eq!(model.attempt, "attempt-b");
        assert_eq!(model.dispatch(), Err(Error::AlreadyDispatched));
        assert_eq!(model.reconcile(), Ok(State::TransferPrepared));
        assert_eq!(model.port.dispatches, 1);
    }
}
