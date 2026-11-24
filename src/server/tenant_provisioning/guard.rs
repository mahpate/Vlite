//! design-tenant-provisioning ITEM-007: the durable target lifecycle guard.
//!
//! One object, `{prefix}lifecycle-guard.json`, beside `authority.json` in a
//! dedicated target's coordination store. While a managed lifecycle
//! operation holds it, neither a manual (`failover execute`) nor an
//! automatic failover may transfer the target's authority, and no writer
//! may claim the authority at startup: the target stays unwritable until
//! the operation has validated it and enables writes (`write_enabled`,
//! which still refuses a transfer). Only the holding operation moves it
//! forward (`held -> write_enabled -> released`, or `held -> released`); a
//! released guard may be held by a later operation. Every write is a
//! compare-and-swap, and every check fails closed: an unreadable or invalid
//! guard refuses like a held one.
//!
//! Not covered (see the PRD, section 15): a writer that already serves is
//! not stopped by a later hold, so the holder must hold before it starts
//! the writer and, after holding, re-read the authority to confirm no
//! transfer passed its check first. The holder side (`hold`,
//! `enable_writes`, `release`) has no production caller yet: the operation
//! worker that composes it is ITEM-010/016.

use super::*;
use crate::server::authority::AuthorityStore;

pub(in crate::server) const OBJECT: &str = "lifecycle-guard.json";
const SCHEMA: &str = "vlite.lifecycle-guard/1";
const MAX_GUARD_BYTES: u64 = 4 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::server) enum State {
    /// No transfer, no writer claim.
    Held,
    /// Validated: a writer may claim; still no transfer.
    WriteEnabled,
    /// The operation ended; nothing is refused.
    Released,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server) struct Guard {
    schema: String,
    pub(in crate::server) tenant: String,
    pub(in crate::server) operation_id: String,
    pub(in crate::server) state: State,
}

/// Why a guard check refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Refusal {
    Reserved,
    WriteDisabled,
    Unavailable,
    Invalid,
}

impl Refusal {
    pub(in crate::server) fn code(self) -> &'static str {
        match self {
            Self::Reserved => "lifecycle_reserved",
            Self::WriteDisabled => "lifecycle_write_disabled",
            Self::Unavailable => "lifecycle_guard_unavailable",
            Self::Invalid => "lifecycle_guard_invalid",
        }
    }

    pub(in crate::server) fn message(self) -> &'static str {
        match self {
            Self::Reserved => {
                "a managed lifecycle operation holds this target; authority cannot be transferred"
            }
            Self::WriteDisabled => {
                "a managed lifecycle operation has not enabled writes on this target"
            }
            Self::Unavailable => "the lifecycle guard could not be read",
            Self::Invalid => "the lifecycle guard is invalid",
        }
    }

    fn error(self) -> ApiError {
        let status = match self {
            Self::Reserved | Self::WriteDisabled => StatusCode::CONFLICT,
            Self::Unavailable | Self::Invalid => StatusCode::SERVICE_UNAVAILABLE,
        };
        ApiError::new(status, self.code(), self.message())
    }
}

/// The guard object of one coordination prefix.
pub(in crate::server) struct Guards {
    store: Arc<dyn ObjectStore>,
    name: String,
}

impl Guards {
    pub(in crate::server) fn new(store: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        Self {
            store,
            name: format!("{prefix}{OBJECT}"),
        }
    }

    /// The guard beside `authority`'s record.
    pub(in crate::server) fn of(authority: &AuthorityStore) -> Self {
        Self::new(authority.objects(), authority.prefix())
    }

    fn read(&self) -> Result<Option<(Guard, String)>, Refusal> {
        let blob = match self.store.get(&self.name, MAX_GUARD_BYTES) {
            Ok(blob) => blob,
            Err(StoreError::NotFound) => return Ok(None),
            Err(StoreError::TooLarge) => return Err(Refusal::Invalid),
            Err(_) => return Err(Refusal::Unavailable),
        };
        let guard: Guard = serde_json::from_slice(&blob.bytes).map_err(|_| Refusal::Invalid)?;
        if guard.schema != SCHEMA || !is_hex(&guard.tenant, 32) || !is_hex(&guard.operation_id, 32)
        {
            return Err(Refusal::Invalid);
        }
        Ok(Some((guard, blob.etag)))
    }

    /// Manual or automatic authority transfer: refused unless no guard
    /// exists or it is released.
    pub(in crate::server) fn check_transfer(&self) -> Result<(), Refusal> {
        match self.read()? {
            Some((guard, _)) if guard.state != State::Released => Err(Refusal::Reserved),
            _ => Ok(()),
        }
    }

    /// A writer's authority claim: refused while the guard is held.
    pub(in crate::server) fn check_write_enable(&self) -> Result<(), Refusal> {
        match self.read()? {
            Some((guard, _)) if guard.state == State::Held => Err(Refusal::WriteDisabled),
            _ => Ok(()),
        }
    }

    pub(in crate::server) fn current(&self) -> Result<Option<Guard>, ApiError> {
        Ok(self.read().map_err(Refusal::error)?.map(|(guard, _)| guard))
    }

    /// Holds the target for `operation_id` (idempotent).
    pub(in crate::server) fn hold(
        &self,
        tenant: &str,
        operation_id: &str,
    ) -> Result<Guard, ApiError> {
        self.move_to(tenant, operation_id, State::Held)
    }

    /// Allows the writer claim once the operation validated the target.
    pub(in crate::server) fn enable_writes(
        &self,
        tenant: &str,
        operation_id: &str,
    ) -> Result<Guard, ApiError> {
        self.move_to(tenant, operation_id, State::WriteEnabled)
    }

    /// Ends the hold (idempotent).
    pub(in crate::server) fn release(
        &self,
        tenant: &str,
        operation_id: &str,
    ) -> Result<Guard, ApiError> {
        self.move_to(tenant, operation_id, State::Released)
    }

    /// Ends an operation whose hold may still be in flight: writes this
    /// operation's `released` guard with a conditional transition over
    /// whatever revision is current (create over absence, compare-and-swap
    /// over a released or this operation's guard). A delayed hold was
    /// conditioned on a revision that preceded this write, so it can no
    /// longer land. Refused while another operation holds the guard; a
    /// write whose outcome is unknown fences nothing (retry it).
    pub(in crate::server) fn fence(
        &self,
        tenant: &str,
        operation_id: &str,
    ) -> Result<Guard, ApiError> {
        if !is_hex(tenant, 32) || !is_hex(operation_id, 32) {
            return Err(invalid("tenant and operation IDs must be 32 lowercase hex"));
        }
        let next = Guard {
            schema: SCHEMA.to_owned(),
            tenant: tenant.to_owned(),
            operation_id: operation_id.to_owned(),
            state: State::Released,
        };
        let current = self.read().map_err(Refusal::error)?;
        let cond = match &current {
            None => Cond::Create,
            // Only a write of this operation's own release can have
            // produced this revision, and it postdates every hold attempt.
            Some((guard, _)) if guard == &next => return Ok(next),
            Some((guard, etag))
                if guard.state == State::Released
                    || (guard.tenant == tenant && guard.operation_id == operation_id) =>
            {
                Cond::Match(etag)
            }
            Some(_) => {
                return Err(conflict(
                    "lifecycle_guard_held",
                    "another operation holds this target's lifecycle guard",
                ));
            }
        };
        self.write(&next, cond)
    }

    fn write(&self, next: &Guard, cond: Cond<'_>) -> Result<Guard, ApiError> {
        let bytes = serde_json::to_vec(next).map_err(|_| state_invalid())?;
        match self.store.put(&self.name, &bytes, cond) {
            Ok(_) => Ok(next.clone()),
            Err(StoreError::Conflict) => Err(conflict(
                "lifecycle_guard_conflict",
                "the lifecycle guard changed concurrently; read it and retry",
            )),
            Err(StoreError::Unknown(_)) => Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "lifecycle_guard_indeterminate",
                "the lifecycle guard write outcome is unknown; retry the same transition",
            )),
            Err(_) => Err(Refusal::Unavailable.error()),
        }
    }

    fn move_to(&self, tenant: &str, operation_id: &str, to: State) -> Result<Guard, ApiError> {
        if !is_hex(tenant, 32) || !is_hex(operation_id, 32) {
            return Err(invalid("tenant and operation IDs must be 32 lowercase hex"));
        }
        let next = Guard {
            schema: SCHEMA.to_owned(),
            tenant: tenant.to_owned(),
            operation_id: operation_id.to_owned(),
            state: to,
        };
        let current = self.read().map_err(Refusal::error)?;
        let cond = match &current {
            None if to == State::Held => Cond::Create,
            None => return Err(not_held()),
            Some((guard, etag)) if guard.tenant == tenant && guard.operation_id == operation_id => {
                if guard.state == to {
                    return Ok(next);
                }
                let forward = matches!(
                    (guard.state, to),
                    (State::Held, State::WriteEnabled)
                        | (State::Held | State::WriteEnabled, State::Released)
                );
                if !forward {
                    return Err(if guard.state == State::Released {
                        ended()
                    } else {
                        conflict(
                            "lifecycle_guard_backwards",
                            "the lifecycle guard only moves forward",
                        )
                    });
                }
                Cond::Match(etag)
            }
            Some((guard, etag)) if guard.state == State::Released && to == State::Held => {
                Cond::Match(etag)
            }
            Some((guard, _)) if guard.state == State::Released => return Err(not_held()),
            Some(_) => {
                return Err(conflict(
                    "lifecycle_guard_held",
                    "another operation holds this target's lifecycle guard",
                ));
            }
        };
        self.write(&next, cond)
    }
}

fn not_held() -> ApiError {
    conflict(
        "lifecycle_guard_not_held",
        "this operation does not hold the target's lifecycle guard",
    )
}

#[cfg(test)]
pub(in crate::server) mod tests {
    use super::*;
    use crate::server::cloud_backup::fake::{Fault, Memory, Op};

    pub(in crate::server) const OPERATION: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";
    pub(in crate::server) const TENANT: &str = "00112233445566778899aabbccddeeff";
    const NAME: &str = "test/lifecycle-guard.json";

    /// Holds the guard under `prefix` (and enables writes if asked).
    pub(in crate::server) fn held(memory: &Arc<Memory>, prefix: &str, write_enabled: bool) {
        let guards = Guards::new(memory.clone(), prefix);
        guards.hold(TENANT, OPERATION).unwrap();
        if write_enabled {
            guards.enable_writes(TENANT, OPERATION).unwrap();
        }
    }

    fn guards(memory: &Arc<Memory>) -> Guards {
        Guards::new(memory.clone(), "test/")
    }

    #[test]
    fn a_hold_refuses_transfer_and_claim_until_released() {
        let memory = Arc::new(Memory::default());
        let guards = guards(&memory);
        assert_eq!(guards.current().unwrap(), None);
        assert_eq!(guards.check_transfer(), Ok(()));
        assert_eq!(guards.check_write_enable(), Ok(()));

        let held = guards.hold(TENANT, OPERATION).unwrap();
        assert_eq!(held.state, State::Held);
        assert_eq!(guards.hold(TENANT, OPERATION).unwrap(), held);
        assert_eq!(guards.check_transfer(), Err(Refusal::Reserved));
        assert_eq!(guards.check_write_enable(), Err(Refusal::WriteDisabled));

        guards.enable_writes(TENANT, OPERATION).unwrap();
        assert_eq!(guards.check_transfer(), Err(Refusal::Reserved));
        assert_eq!(guards.check_write_enable(), Ok(()));

        guards.release(TENANT, OPERATION).unwrap();
        guards.release(TENANT, OPERATION).unwrap();
        assert_eq!(guards.check_transfer(), Ok(()));
        assert_eq!(guards.check_write_enable(), Ok(()));
        // A later operation may hold the released target again.
        guards.hold(TENANT, OTHER).unwrap();
        assert_eq!(guards.check_transfer(), Err(Refusal::Reserved));
    }

    #[test]
    fn only_the_holder_moves_it_and_only_forward() {
        let memory = Arc::new(Memory::default());
        let guards = guards(&memory);
        assert_eq!(
            guards.enable_writes(TENANT, OPERATION).unwrap_err().code,
            "lifecycle_guard_not_held"
        );
        assert_eq!(
            guards.release(TENANT, OPERATION).unwrap_err().code,
            "lifecycle_guard_not_held"
        );
        guards.hold(TENANT, OPERATION).unwrap();
        for (result, code) in [
            (guards.hold(TENANT, OTHER), "lifecycle_guard_held"),
            (guards.enable_writes(TENANT, OTHER), "lifecycle_guard_held"),
            (guards.release(TENANT, OTHER), "lifecycle_guard_held"),
            (guards.release(OTHER, OPERATION), "lifecycle_guard_held"),
        ] {
            assert_eq!(result.unwrap_err().code, code);
        }
        guards.enable_writes(TENANT, OPERATION).unwrap();
        assert_eq!(
            guards.hold(TENANT, OPERATION).unwrap_err().code,
            "lifecycle_guard_backwards"
        );
        guards.release(TENANT, OPERATION).unwrap();
        // An ended operation cannot hold or enable again; nor can another
        // operation enable or release a guard it never held.
        for result in [
            guards.hold(TENANT, OPERATION),
            guards.enable_writes(TENANT, OPERATION),
        ] {
            assert_eq!(result.unwrap_err().code, "operation_terminal");
        }
        for result in [
            guards.enable_writes(TENANT, OTHER),
            guards.release(TENANT, OTHER),
        ] {
            assert_eq!(result.unwrap_err().code, "lifecycle_guard_not_held");
        }
        assert_eq!(guards.check_transfer(), Ok(()));
        assert_eq!(
            guards.hold("not-hex", OTHER).unwrap_err().code,
            "operation_request_invalid"
        );
    }

    #[test]
    fn writes_are_compare_and_swap() {
        let memory = Arc::new(Memory::default());
        let guards = guards(&memory);
        let racer = Guards::new(memory.clone(), "test/");
        memory.hook(Op::Put, NAME, move || {
            racer.hold(TENANT, OTHER).unwrap();
        });
        assert_eq!(
            guards.hold(TENANT, OPERATION).unwrap_err().code,
            "lifecycle_guard_conflict"
        );
        assert_eq!(guards.current().unwrap().unwrap().operation_id, OTHER);
        // An unknown outcome is retried as the same transition.
        memory.fault(Op::Put, NAME, Fault::After, 0, 1);
        assert_eq!(
            guards.enable_writes(TENANT, OTHER).unwrap_err().code,
            "lifecycle_guard_indeterminate"
        );
        assert_eq!(
            guards.enable_writes(TENANT, OTHER).unwrap().state,
            State::WriteEnabled
        );
    }

    #[test]
    fn a_fence_invalidates_every_earlier_conditional_hold() {
        let memory = Arc::new(Memory::default());
        let guards = guards(&memory);
        let bytes = |op: &str| {
            serde_json::to_vec(&Guard {
                schema: SCHEMA.into(),
                tenant: TENANT.into(),
                operation_id: op.into(),
                state: State::Held,
            })
            .unwrap()
        };
        // Over absence: a delayed create-only hold conflicts afterwards.
        assert_eq!(
            guards.fence(TENANT, OPERATION).unwrap().state,
            State::Released
        );
        assert_eq!(
            memory.put(NAME, &bytes(OPERATION), Cond::Create),
            Err(StoreError::Conflict)
        );
        // Idempotent over its own release, which postdates every hold.
        guards.fence(TENANT, OPERATION).unwrap();
        // Over a predecessor's release: a delayed hold matched on that
        // revision conflicts afterwards.
        guards.hold(TENANT, OTHER).unwrap();
        guards.release(TENANT, OTHER).unwrap();
        let old = memory.get(NAME, MAX_GUARD_BYTES).unwrap().etag;
        guards.fence(TENANT, OPERATION).unwrap();
        assert_eq!(
            memory.put(NAME, &bytes(OPERATION), Cond::Match(&old)),
            Err(StoreError::Conflict)
        );
        // Its own hold is released by the fence.
        let fresh = Arc::new(Memory::default());
        let own = Guards::new(fresh.clone(), "test/");
        own.hold(TENANT, OPERATION).unwrap();
        own.fence(TENANT, OPERATION).unwrap();
        assert_eq!(own.current().unwrap().unwrap().state, State::Released);
        // Never over another operation's hold; an unknown write fences
        // nothing and is retried.
        guards.hold(TENANT, OTHER).unwrap();
        assert_eq!(
            guards.fence(TENANT, OPERATION).unwrap_err().code,
            "lifecycle_guard_held"
        );
        guards.release(TENANT, OTHER).unwrap();
        memory.fault(Op::Put, NAME, Fault::Before, 0, 1);
        assert_eq!(
            guards.fence(TENANT, OPERATION).unwrap_err().code,
            "lifecycle_guard_indeterminate"
        );
        assert_eq!(guards.current().unwrap().unwrap().operation_id, OTHER);
        guards.fence(TENANT, OPERATION).unwrap();
        assert_eq!(guards.current().unwrap().unwrap().operation_id, OPERATION);
    }

    #[test]
    fn checks_fail_closed_on_an_unreadable_or_invalid_guard() {
        let memory = Arc::new(Memory::default());
        let guards = guards(&memory);
        guards.hold(TENANT, OPERATION).unwrap();
        guards.release(TENANT, OPERATION).unwrap();
        let good: Value = serde_json::from_slice(&memory.raw(NAME).unwrap()).unwrap();
        let mut cases = Vec::new();
        for (field, value) in [
            ("schema", json!("vlite.lifecycle-guard/2")),
            ("tenant", json!("x")),
            ("operation_id", json!(OPERATION.to_uppercase())),
            ("state", json!("unknown")),
            ("extra", json!(1)),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            cases.push(serde_json::to_vec(&bad).unwrap());
        }
        cases.push(b"{}".to_vec());
        cases.push(vec![b' '; MAX_GUARD_BYTES as usize + 1]);
        for bytes in cases {
            memory.set_raw(NAME, &bytes);
            assert_eq!(guards.check_transfer(), Err(Refusal::Invalid));
            assert_eq!(guards.check_write_enable(), Err(Refusal::Invalid));
            assert_eq!(
                guards.hold(TENANT, OTHER).unwrap_err().code,
                "lifecycle_guard_invalid"
            );
        }
        memory.set_raw(NAME, &serde_json::to_vec(&good).unwrap());
        memory.fault(Op::Get, NAME, Fault::Refuse, 0, 2);
        assert_eq!(guards.check_transfer(), Err(Refusal::Unavailable));
        assert_eq!(guards.check_write_enable(), Err(Refusal::Unavailable));
        assert_eq!(guards.check_transfer(), Ok(()));
    }
}
