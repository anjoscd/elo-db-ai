# Evaluation method (v0.2)

ELO is built against a benchmark, not against intuition. Every component reports its effect on the metrics below. Every claim in the README must trace to a run of `elo bench`, and until a run exists, README statements are goals, not results.

## 1. Fixture and labels

- **Public fixture (in this repo):** a synthetic workspace with 3–4 small projects, two declared links, around 150 items (laws, rules, decisions with supersession chains, notes) and around 300 events (prompts and tool events) in session sequences.
- **Labels are independent of the resolver.** For each event, annotators record:
  - `allowed_scopes`: the scopes that legitimately apply, decided from the fixture's story, not from ELO's output;
  - `required`: items whose **content** the agent needs for this event;
  - `forbidden`: items that would mislead, such as wrong project, superseded or expired;
  - everything else is `neutral`.
- Labels are written before any tuning and changed only in separate, reviewed commits. Two annotators label a 20% overlap, and their agreement (Cohen's κ) is reported.
- **Generality:** besides the main fixture, at least 5 public open-source repositories of different shapes (single package, JS monorepo, Rust workspace, polyglot services, Python) are used **unmodified** to test project and link discovery. For each, labels give the true projects and true links. Metrics: project discovery precision/recall and link suggestion precision@k. No per-repository configuration is allowed in these runs.
- **Private fixtures (never committed):** users can run the same harness on their own workspace. Only aggregate numbers may be shared.

## 2. Metrics (computed per event, then macro-averaged; events with an empty denominator are excluded from that metric and counted separately)

### Injection
| Metric | Definition |
|---|---|
| **Forbidden rate** | events with ≥ 1 forbidden item emitted in full or as a pointer ÷ events |
| **Scope violation rate** | emitted items whose scope ∉ `allowed_scopes` ÷ emitted items |
| **Relevance precision** | full-body items ∈ `required` ÷ full-body items emitted |
| **Required full recall** | `required` items emitted in full ÷ `required` |
| **Required reachable recall** | `required` items emitted in full **or** named by id in a pointer ÷ `required`. Reported separately and never merged with full recall. |
| **Required session recall** | `required` items emitted in full in this event **or earlier in the same session and compaction window** ÷ `required`. This is the same credit laws get. Added in v0.2 because SPEC §4.3 forbids re-serving delivered items, so per-event full recall penalizes correct dedup. |
| **Law full coverage** | applicable laws whose **body** was emitted in this event, or earlier in the same session and compaction window (SPEC §4.4) ÷ applicable laws. A title, reminder line or overflow line does **not** count as the first delivery. |
| **Supersession leaks** | superseded or expired items emitted or returned by default recall (count) |
| **Bytes per event** | p50 / p95 of `used` |
| **Abstention accuracy** | events with empty `required` where ELO emitted nothing (except laws) ÷ such events |

### Code map (per edge type: `imports`, `calls`, `renders`, `registers_callback`, reported separately)
| Metric | Definition |
|---|---|
| **Edge precision / recall** | against hand-labelled expected edges |
| **False zeros** | queries answering resolved = 0 **and** unresolved = 0 when expected ≥ 1 |
| **Answer coverage** | queries with ≥ 1 resolved result when expected ≥ 1 ÷ such queries. This stops "always unknown" from scoring well. |

### Calibration (after M1, SPEC §6.4)
| Metric | Definition | Ship gate |
|---|---|---|
| **ECE** | expected calibration error of `p_useful` vs `required`, 10 equal-width bins, on `test` | ≤ 0.05 |
| **Brier score** | mean squared error of `p_useful` | ≤ that of the uncalibrated min-max-scaled score |
| **Reliability diagram** | predicted vs observed rate per bin, published with each report | n/a |

### Latency
p50/p95, cold and warm, on the reference machine (SPEC §10).

## 3. Baseline and pass criteria

- **Primary baseline (B1):** an item matches if any of its `triggers.keywords` appears in the prompt as a whole word (case-insensitive, accent-insensitive). Title and body are not searched. Matching runs over **all** items regardless of scope or status. Items are ordered by `updated_at desc`, then `id asc`, and emitted in full until the next item wouldn't fit within the same `budget_bytes`. Its implementation lives in `bench/baselines/` and is the reference for every delta.
- **Secondary baseline (B0):** inject nothing.
- **M1 pass criteria (on `test`):**
  - forbidden rate = 0 and scope violation rate = 0;
  - law full coverage = 1.00 (laws are capped in size, see SPEC §4.4, so this is achievable);
  - required reachable recall ≥ min(B1 + 0.20, 0.95) **and** required session recall ≥ B1 (per-event full recall is reported but not gated, because SPEC §4.3 forbids re-serving bodies; re-relevant delivered items must appear as reminders, which count as reachable);
  - relevance precision ≥ min(B1 + 0.20, 0.95);
  - bytes p95 ≤ B1;
  - supersession leaks = 0.

## 4. Protocol

1. **Split:** tune weights on `dev`, report on `test`. Never tune on `test`.
2. **Ablations:** remove links, quotas, pointers, recency or idf one at a time and report the delta. A feature without a positive delta is removed or kept off by default.
3. **Determinism:** fixed `now`, a fixed initial session state and a fixed event order. The same inputs produce byte-identical outputs, and CI fails on drift.
4. **Report:** `bench/results/<date>-<commit>.json` plus a Markdown summary. The README links the latest one.

## 5. Task-level evaluation (optional, after M1)

A small set of coding tasks run by an agent under B0, B1 and ELO, with at least 3 runs per arm, reporting success, rule violations, steps and **tokens actually consumed** (bytes are only a proxy). Variance is reported, and no claim is made from a single run.

## 6. What we don't claim

- ELO does not make a weak model write better code. Its goal is to deliver the right constraints, avoid misleading context, and do it at a lower cost, and each of those is a hypothesis to be measured.
- `active` status means "not superseded in the store", not "true".
- A structural database edge is not an access proof.

## 7. Live shadow comparison against an incumbent engine

Before ELO replaces another context engine in a real workspace, both run side by side: the incumbent keeps injecting, and ELO runs with `elo hook <event> --shadow`, recording what it would inject under `.elo/shadow/`. Criteria are fixed **before** looking at the data:

- at least 7 days and 150 real prompts;
- cross-project injections: 0% of prompts;
- applicable laws delivered: 100%;
- silent cuts (a relevant item that is neither injected nor pointed to): 0;
- bytes per prompt ≤ the incumbent's;
- hook latency p95 < 100 ms;
- blind review: about 20 prompts sampled at random, both injections shown as unlabeled A/B, and ELO wins or ties in ≥ 70%.

The comparison is only fair when ELO holds the same knowledge as the incumbent (rules **and** current decisions and notes). Shadow logs and imported knowledge stay in the workspace and never go into this repository.
