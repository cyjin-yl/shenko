# Runtime resource reachability — 2026-11-15 continuation

This is a read-only inspection for the continuation after candidate
`299447344bad7c3d844a9f2411963c50f803f0b5`. It is not a real model/OXR acceptance
run and does not invalidate historical measurements.

The checked-in [model harness setup](../development/local-model-behavior.md)
names `http://10.10.10.11:8000`; historical build evidence names
`root@10.10.10.12`, Docker container `shenko-dev-persistent-20261026` and project
`/tank/shared/projects/shenko`. The earlier model/seat records report their
measured configuration at that time. They do not establish the current model,
route or engine state.

## Observations in this session

| Surface | Read-only observation |
| --- | --- |
| Assigned Box | One running box named `main` was returned by the Box inventory. No machine was created or changed. |
| Tailscale on Box main | `NeedsLogin`, no assigned Tailscale address, no peers; reported network unavailable. No login/network change was attempted. |
| Documented model URL through the existing proxy | GET `/v1/models` returned HTTP 502. No generation or model mutation was requested. |
| Direct documented model route | `ip route get 10.10.10.11` returned network unreachable. A per-request direct GET failed immediately with curl exit 7 / HTTP 000. |
| Documented SSH host | A bounded TCP connection to `10.10.10.12:22` failed with network unreachable. No SSH login or host-key change was attempted. |
| Known Box SSH configuration | Only the GitHub host entry was present. Private key material was not read or emitted. |
| Shenko setup on Box main | The documented project path and normal Shenko configuration/data locations were absent; no Shenko/model/OXR-specific environment names were present. Values were not printed. |
| Local executor setup | Normal Shenko configuration/data locations and matching model/OXR environment names were absent. |

The repository's only documented model address is that private endpoint. The
public free OXR URL in the provider research is API documentation, not a bound
account or permission to upload user documents. No production message was sent,
service changed, model downloaded, credential printed or external parse launched.

## What remains executable when the route/binding is available

The actual local-model behavior harness requires the reachable configured
endpoint and its real model identity. Its existing repeated task/awareness
journeys must run through the actual prompt, provider and parser; an environment
skip is not an acceptance result. Configured OXR requires its endpoint, model,
account identity and credential, then an actual document journey. Channel and
seat acceptance likewise require their real provisioned service/session; the
native loopback and restricted helper proofs establish narrower mechanisms.

These observations establish the routes and bindings visible to this session.
They do not prove that credentials do not exist elsewhere, that remote engines
are stopped, or that the historical endpoints have been decommissioned. The
remaining prerequisite is a reachable authorized route and exact service
binding, followed by retained real journey results and the required long run.
