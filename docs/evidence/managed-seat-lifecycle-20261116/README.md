# Managed seat ownership and teardown candidate

The original authored source is `e4d45747d53f7701aea4a293310192dac2126205`,
followed by `95e0a2d68b459f3cfee9561d5211a40b3d6a1499`. Both checkpoints,
including their original author and time, are preserved in the
[source archives](../source-checkpoints-20261116/README.md).

`source-check.json` retains the first checkpoint's actual static checks.
`followup-source-check.json` binds the later correction without overwriting that
record. Neither claims a Rust compilation or an actual OMP/Claude coding run.

Independent source review found two classes of defect in the initial candidate:
close errors could remain only in a cancelled closer's local variables, and
seven nested Cordis spawns created independent resident Fibers rather than
registrations owned by the seat generation. The follow-up saves each observed
failure synchronously in its owner before later awaits and composes the seven
existing NativeTaskHandlerPlugin registrations into the same generation journal.
The reviewer rechecked the exact follow-up and found no remaining confirmed
blocker in those reviewed paths. The root integration also compared every
changed source blob against that reviewed original.

The [current contract](../../specs/remote-seats.md#managed-transport-lifecycle-implementation)
describes process/reader ownership, original pending-write identity and Unknown,
bounded PTY shutdown, and retained failures. The increment contains six new or
extended observation windows, including one actual Cordis restart, disposal and
fresh-install journey. They are authored but not yet executed at this checkpoint.
This is not evidence of native seat.submit, a persistent coding companion, full
cross-process writer fencing, internal model-budget accounting, or a tested
self-modification/activation loop.
