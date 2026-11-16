# Runtime resource reachability — 2026-11-15 continuation

The initial section records a read-only inspection for the continuation after candidate
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

At that earlier checkpoint, the model address identified by the deployment guide was the private endpoint above; the later upstream evidence below reveals another route. The
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

## Additional installed model assets discovered after the upstream merge

The newly merged [Obelisk evidence](https://github.com/cyjin-yl/shenko/blob/bfc3d04de445968dcaed3f40cb8a937da437f61e/docs/evidence/2026-11-16-obelisk-qwen/INDEX.json) identifies a separate authorized route: a Box-local CPU llama.cpp service, accessed through the existing Box connector and an owned scratch relay. The historical 4B service was deliberately stopped after its runner ended, with owned SIGTERM at 18:10:53 UTC and exit 0 at 18:10:54 UTC. Its stopped state is not missing credentials or missing assets.

Both existing GGUF files and llama.cpp were independently inspected. The files are Qwen3-4B-Q4_K_M (2,497,280,256 bytes) and Qwen3-1.7B-Q8_0 (1,834,426,016 bytes), each with its embedded tokenizer and checked expected SHA256. The server reports `b12359 / b1b770d90d94cad9e9ea5b993bdbc383a522fb28`.

Two isolated owned preflights used an unchanged 8,192-token context, q8_0 KV, 448 MiB runtime reserve and an additional 512 MiB available-memory margin. Each refused before launching the model child:

| Existing model | Observation UTC | Required available bytes, including margin | Actual MemAvailable at refusal | Result |
| --- | --- | ---: | ---: | --- |
| Qwen3-4B-Q4_K_M | 18:56:14–18:56:18 | 4,145,641,728 | 3,911,204 KiB, after two higher observations | No model child started |
| Qwen3-1.7B-Q8_0 | 18:57:18 | 3,340,181,152 | 3,232,140 KiB | No model child started |

No other workload was stopped, no swap or network configuration changed, and no threshold was reduced to force execution. At 18:59:01 UTC, GET `/v1/models` on Box localhost ports 18831, 18834 and 18836 returned connection refused. Both owned supervisors had exited; neither created a model process or server log. The actual totals are **zero Cargo invocations, zero model children and zero inference requests** for this resource attempt.

The planned three existing protocol smoke cases therefore remain **not run**. There is no pass, success rate or skip-as-pass. The scoped prompt/provider/parser source was unchanged between `45cb2b7` and the fixture-only `3936e16`; neither model would establish the specified 27B baseline, awareness efficacy or a full Store-to-restart product journey even if those narrow cases had run.

Retained structured summary SHA256: `28a9b5d3da1cedabf54f65ae5eede84567033dbfa6e44ed4ebcc5c6257e4f1ad`; complete preflight evidence SHA256: `17f772cecf0a8b7b681d9e1af4714a4ac64d032843bd07a7efb4d9531e4e0fdc`. The 4B supervisor log SHA256 is `95030cfd42ed96a6edd13ac733e7b961da7f1033f40ccf051c599635df6ef969`; the 1.7B log SHA256 is `c55b41f405618aab930fd57d81faf960d5baf4cb7ede6d71f68b4d538dc90e2f`. Resource plans and observed counters are included in the continuation JSON. This records the inspected assets and measured capacity at those instants; it does not assert that no other model or capacity could ever be available.
