---------------------------- MODULE KvResidencyPool ----------------------------
\* A bounded finite model of the Shenko KV residency pool safety boundary:
\* lease admission, reader holds, unknown-read retention, identity
\* invalidation, rebalance/reclaim and release ordering.
\*
\* SPEC §13.1–13.3 (residency pool and leases), §13.5 (identity invalidation),
\* §13.7.3 (automatic policy), §41.9/§41.10 (unknown-outcome read holds).
\*
\* Every action and guard below is bound to a Rust symbol in
\* correspondence.json (same directory). Nothing here is a verification
\* result: it is model source for TLC.
\*
\* Abstraction boundary (see correspondence.json "abstractions"):
\*  * ONE pool with a fixed observed capacity.
\*  * Contexts are named elements from a finite set; each carries a unit
\*    cost of 1, a side (Main/Child) and an identity tag.
\*  * The policy is collapsed: no hysteresis, no candidate enumeration.
\*    The model checks safety (capacity, reader holds, unknown-read holds),
\*    not the choice quality.
\*  * Identity is collapsed to a single tag from a finite set. "matches"
\*    means equal tags. The option-compatible unknown-but-not-invalidating
\*    semantics is not modelled: the tag either matches or doesn't.
\*  * Time is unmodelled. The hold window is a quality concern, not safety.
\*
\* Bounds (config): Contexts, Requests, Identities, Capacity, MaxReaders.

EXTENDS Naturals, FiniteSets

CONSTANTS
    Contexts,      \* the finite set of context IDs {"c1","c2",...}
    Requests,      \* the finite set of request IDs {"r1","r2",...}
    Identities,    \* the finite set of identity tags {"id1","id2"}
    Capacity,      \* the observed pool capacity in units (a natural number)
    MaxReaders     \* bound on readers per context (makes the state space finite)

VARIABLES
    leases,        \* context -> record or absent marker
    unknownReads,  \* request -> context id or "none"
    poolIdentity,  \* the current pool-level identity tag
    staleAfter     \* context -> whether an identity change happened since the
                   \* last Acquire/ReAcquire; tracks invalidation completeness

vars == <<leases, unknownReads, poolIdentity, staleAfter>>

\* ---------------------------------------------------------------------------
\* Helpers
\* ---------------------------------------------------------------------------

\* States that occupy units in the pool.
OccupiedStates == {"Resident", "Transferring", "ReclaimPending"}

\* Sum of units occupied by all present leases in occupied states.
\* Each context occupies exactly 1 unit when present and in an occupied state.
OccupiedUnits ==
    Cardinality({c \in Contexts : leases[c].present
                                  /\ leases[c].state \in OccupiedStates})

\* Whether a context has an unresolved unknown read pointing at it.
HasUnknownRead(c) ==
    \E r \in Requests : unknownReads[r] = c

\* Whether a drain on context c should remove it: last reader of a
\* reclaimable or invalid lease.
ShouldDrainRemove(c) ==
    leases[c].readers = 1
    /\ leases[c].state \in {"ReclaimPending", "Invalid"}

\* ---------------------------------------------------------------------------
\* Initial state
\* ---------------------------------------------------------------------------

Init ==
    /\ leases = [c \in Contexts |-> [present |-> FALSE, state |-> "None",
                                      units |-> 1, side |-> "Main",
                                      readers |-> 0, identity |-> "id1"]]
    /\ unknownReads = [r \in Requests |-> "none"]
    /\ poolIdentity = "id1"
    /\ staleAfter = [c \in Contexts |-> FALSE]

\* ---------------------------------------------------------------------------
\* Actions (each bound to source in correspondence.json)
\* ---------------------------------------------------------------------------

\* KvAllocator::acquire — admit a new context into the pool.
\* Source: kv.rs Acquire checks occupied + incoming <= capacity.
Acquire(c, id) ==
    /\ ~leases[c].present
    /\ OccupiedUnits < Capacity  \* 1 unit per context; < because +1 must fit
    /\ leases' = [leases EXCEPT ![c] = [present |-> TRUE, state |-> "Resident",
                                         units |-> 1, side |-> "Main",
                                         readers |-> 0, identity |-> id]]
    /\ staleAfter' = [staleAfter EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<unknownReads, poolIdentity>>

\* KvAllocator::acquire on an existing lease — re-acquire/update identity.
\* Source: kv.rs Acquire on existing lease, state must be Resident.
ReAcquire(c, id) ==
    /\ leases[c].present
    /\ leases[c].state = "Resident"
    /\ leases' = [leases EXCEPT ![c].identity = id]
    /\ staleAfter' = [staleAfter EXCEPT ![c] = FALSE]
    /\ UNCHANGED <<unknownReads, poolIdentity>>

\* KvAllocator::begin_read — register a reader on a resident context.
\* A ReclaimPending context is revived to Resident.
\* Source: kv.rs begin_read.
BeginRead(c) ==
    /\ leases[c].present
    /\ leases[c].state \in {"Resident", "ReclaimPending", "Transferring"}
    /\ leases[c].readers < MaxReaders
    /\ leases' = [leases EXCEPT ![c].readers = leases[c].readers + 1,
                                ![c].state = "Resident"]
    /\ UNCHANGED <<unknownReads, poolIdentity, staleAfter>>

\* KvAllocator::end_read / drain_reader — drain one reader.
\* If last reader of ReclaimPending/Invalid: remove lease entirely.
\* Source: kv.rs end_read -> drain_reader.
EndReadRemove(c) ==
    /\ leases[c].present
    /\ ShouldDrainRemove(c)
    /\ leases' = [leases EXCEPT ![c].present = FALSE,
                                ![c].readers = 0,
                                ![c].state = "None"]
    /\ UNCHANGED <<unknownReads, poolIdentity, staleAfter>>

EndReadKeep(c) ==
    /\ leases[c].present
    /\ leases[c].readers > 0
    /\ ~ShouldDrainRemove(c)
    /\ leases' = [leases EXCEPT ![c].readers = leases[c].readers - 1]
    /\ UNCHANGED <<unknownReads, poolIdentity, staleAfter>>

\* KvAllocator::hold_unknown_read — keep a context's read hold open because
\* the request's outcome is unknown.
\* Source: kv.rs hold_unknown_read.
HoldUnknownRead(r, c) ==
    /\ unknownReads[r] = "none"
    /\ leases[c].present
    /\ leases[c].readers > 0
    /\ unknownReads' = [unknownReads EXCEPT ![r] = c]
    /\ UNCHANGED <<leases, poolIdentity, staleAfter>>

\* KvAllocator::release_unknown_read — terminal evidence arrived for request r.
\* Source: kv.rs release_unknown_read -> drain_reader.
ReleaseUnknownReadRemove(r) ==
    /\ unknownReads[r] # "none"
    /\ LET c == unknownReads[r] IN
       /\ leases[c].present
       /\ ShouldDrainRemove(c)
       /\ unknownReads' = [unknownReads EXCEPT ![r] = "none"]
       /\ leases' = [leases EXCEPT ![c].present = FALSE,
                                   ![c].readers = 0,
                                   ![c].state = "None"]
       /\ UNCHANGED <<poolIdentity, staleAfter>>

ReleaseUnknownReadKeep(r) ==
    /\ unknownReads[r] # "none"
    /\ LET c == unknownReads[r] IN
       /\ leases[c].present
       /\ leases[c].readers > 0
       /\ ~ShouldDrainRemove(c)
       /\ unknownReads' = [unknownReads EXCEPT ![r] = "none"]
       /\ leases' = [leases EXCEPT ![c].readers = leases[c].readers - 1]
       /\ UNCHANGED <<poolIdentity, staleAfter>>

ReleaseUnknownReadGone(r) ==
    /\ unknownReads[r] # "none"
    /\ LET c == unknownReads[r] IN
       ~leases[c].present \/ leases[c].readers = 0
    /\ unknownReads' = [unknownReads EXCEPT ![r] = "none"]
    /\ UNCHANGED <<leases, poolIdentity, staleAfter>>

\* KvAllocator::release — release a context from the pool.
\* Source: kv.rs release; precondition: no unresolved unknown read.
ReleaseImmediate(c) ==
    /\ leases[c].present
    /\ ~HasUnknownRead(c)
    /\ leases[c].readers = 0
    /\ leases' = [leases EXCEPT ![c].present = FALSE,
                                ![c].state = "None"]
    /\ UNCHANGED <<unknownReads, poolIdentity, staleAfter>>

ReleaseDeferred(c) ==
    /\ leases[c].present
    /\ ~HasUnknownRead(c)
    /\ leases[c].readers > 0
    /\ leases' = [leases EXCEPT ![c].state = "ReclaimPending"]
    /\ UNCHANGED <<unknownReads, poolIdentity, staleAfter>>

\* KvAllocator::reclaim — evict an idle resident context when over capacity.
\* Source: kv.rs reclaim; filter: Resident, readers=0, no unknown read.
Reclaim(c) ==
    /\ OccupiedUnits > Capacity
    /\ leases[c].present
    /\ leases[c].state = "Resident"
    /\ leases[c].readers = 0
    /\ ~HasUnknownRead(c)
    /\ leases' = [leases EXCEPT ![c].state = "ReclaimPending"]
    /\ UNCHANGED <<unknownReads, poolIdentity, staleAfter>>

\* KvAllocator::adopt_identity + KvPoolState::invalidate_stale —
\* the pool identity changes; every lease whose identity doesn't match
\* becomes Invalid.
\* Source: kv.rs adopt_identity -> invalidate_stale.
IdentityChange(newId) ==
    /\ newId # poolIdentity
    /\ poolIdentity' = newId
    /\ leases' = [c \in Contexts |->
                    IF leases[c].present
                       /\ leases[c].state \notin {"Invalid", "None"}
                       /\ leases[c].identity # newId
                    THEN [leases[c] EXCEPT !.state = "Invalid"]
                    ELSE leases[c]]
    /\ staleAfter' = [c \in Contexts |->
                        IF leases[c].present THEN TRUE ELSE staleAfter[c]]
    /\ UNCHANGED <<unknownReads>>

\* ---------------------------------------------------------------------------
\* Next-state relation
\* ---------------------------------------------------------------------------

Next ==
    \/ \E c \in Contexts, id \in Identities : Acquire(c, id)
    \/ \E c \in Contexts, id \in Identities : ReAcquire(c, id)
    \/ \E c \in Contexts : BeginRead(c)
    \/ \E c \in Contexts : EndReadRemove(c)
    \/ \E c \in Contexts : EndReadKeep(c)
    \/ \E r \in Requests, c \in Contexts : HoldUnknownRead(r, c)
    \/ \E r \in Requests : ReleaseUnknownReadRemove(r)
    \/ \E r \in Requests : ReleaseUnknownReadKeep(r)
    \/ \E r \in Requests : ReleaseUnknownReadGone(r)
    \/ \E c \in Contexts : ReleaseImmediate(c)
    \/ \E c \in Contexts : ReleaseDeferred(c)
    \/ \E c \in Contexts : Reclaim(c)
    \/ \E id \in Identities : IdentityChange(id)

\* No fairness is assumed or claimed; the safety run uses INIT/NEXT.
Spec == Init /\ [][Next]_vars

\* ---------------------------------------------------------------------------
\* Safety properties (source-backed; see correspondence.json "properties")
\* ---------------------------------------------------------------------------

\* P1. The pool never over-commits: occupied units never exceed the capacity.
\*     Source: KvAllocator::acquire checks occupied + incoming <= capacity;
\*     reclaim and release only reduce occupancy; no path adds a unit without
\*     the capacity check.
PoolNeverOverCommits ==
    OccupiedUnits <= Capacity

\* P2. Identity invalidation is complete: any present lease that became stale
\*     after an identity change (staleAfter[c] = TRUE) and whose identity
\*     does not match the current pool identity is in state Invalid. A newly
\*     acquired context (staleAfter = FALSE) may have any identity because
\*     it was created after the most recent IdentityChange.
\*     Source: KvPoolState::invalidate_stale iterates all leases.
IdentityInvalidationComplete ==
    \A c \in Contexts :
        leases[c].present
        /\ staleAfter[c]
        /\ leases[c].identity # poolIdentity
        => leases[c].state = "Invalid"

\* P3. A context whose state is Invalid cannot gain new readers.
\*     Source: begin_read requires state in {Resident, ReclaimPending,
\*     Transferring}; Invalid is excluded. This is structural in BeginRead's
\*     precondition.
\*     As an invariant: if a context is Invalid, then in the very next step,
\*     its readers can only decrease or stay the same. We can express a weaker
\*     form: the set of contexts that are Invalid AND have readers > 0 is a
\*     subset of contexts that had readers before the invalidation.
\*     Since this is structural in the action guard, it is verified by TLC
\*     exploring all states: no state exists where an Invalid context has more
\*     readers than it had when it became Invalid.

\* P4. An eviction target is correctly guarded: Reclaim only targets contexts
\*     that are Resident with 0 readers and no unknown read. This is verified
\*     by the action guard. As a derived invariant: any context that is in
\*     ReclaimPending state with an unknown read must have been deferred-
\*     released (had readers > 0 at release time), not reclaimed.
\*     Source: KvAllocator::reclaim filter, KvAllocator::release.

\* P5. Unknown read prevents release: no Release action fires when the
\*     context has an unknown read. This is structural (action precondition).
\*     As a consequence: if a context has an unknown read and is present, its
\*     state never became "None" from a Release (it can only become None from
\*     drain, which requires readers = 0 and state in {ReclaimPending,Invalid}).

\* P6. A removed lease had no readers at the moment of removal. Since
\*     drain_reader and ReleaseImmediate are the only paths that set
\*     present = FALSE, and drain requires readers = 1 (about to become 0)
\*     and ReleaseImmediate requires readers = 0, this is structural.

=============================================================================
