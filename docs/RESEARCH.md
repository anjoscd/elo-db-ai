# Research notes

Sources behind ELO's design. Each entry gives the finding we rely on and what ELO does with it. Entries marked *(summary only)* were read through abstracts or secondary coverage and still need a full read.

**How to read this page:** each finding holds for the setting its authors studied (their repositories, models and tasks). ELO treats these findings as **hypotheses to test on its own benchmark**, not as proof that ELO works.

## Context selection and context files

- **Stratum: Repository Memory for Coding Agents Beyond the Context Window**, arXiv [2610.36153](https://arxiv.org/abs/2610.36153), Oct 2026 *(summary only)*. Finding: in an analysis of 752 bug-fix commits, the bottleneck is selection, not capacity. → ELO: scored selection with quotas (SPEC §4).
- **Do Context Files Help Coding Agents? A Two-Agent Ablation Study on Real Repositories**, arXiv [2607.27250](https://arxiv.org/abs/2607.27250), Jul 2026 *(summary only)*. Finding: generic context files did not measurably improve correctness on the repositories tested. → ELO: abstention, with no filler (SPEC §4.6).
- **Evaluating AGENTS.md: Are Repository-Level Context Files Helpful for Coding Agents?**, ETH Zurich, arXiv [2602.11988](https://arxiv.org/abs/2602.11988), Feb 2026, covered by [InfoQ](https://www.infoq.com/news/2026/03/agents-context-file-value-review/) *(summary only)*. Finding: context files often add steps and cost (> 20%) and can hinder agents. → ELO: measure bytes per event, inject less.
- **SWE Context Bench: A Benchmark for Context Learning in Coding**, arXiv [2602.08316](https://arxiv.org/abs/2602.08316), Feb 2026 *(summary only)*. Benchmark for context reuse in coding. → candidate for task-level evaluation.
- **Context Rot**, Chroma Research, [Jul 2025](https://trychroma.com/research/context-rot). Finding: performance degrades with input length across 18 models, and a single distractor hurts. → ELO: cross-project items are distractors, so scope gating comes first.

## Memory, validity and supersession

- **Supersede: Diagnosing and Training the Memory-Update Gap in LLM Agents**, arXiv [2606.27472](https://arxiv.org/abs/2606.27472), Jun 2026 *(summary only)*. Finding: agents fail to *maintain* updates under bounded memory. → ELO: explicit supersession, default retrieval sees only active items (SPEC §5).
- **DreamBench-SWE: A Multi-Session Memory-Hygiene Benchmark for Software Agents**, arXiv [2608.20664](https://arxiv.org/abs/2608.20664), Aug 2026 *(title only, to read)*.
- **Zep: A Temporal Knowledge Graph Architecture for Agent Memory**, arXiv [2501.13956](https://arxiv.org/abs/2501.13956), Jan 2025. Bi-temporal edges and invalidation instead of deletion. → ELO: validity fields and tombstones.
- **Mem0: Building Production-Ready AI Agents with Scalable Long-Term Memory**, arXiv [2504.19413](https://arxiv.org/abs/2504.19413), Apr 2025. ADD/UPDATE/DELETE/NOOP consolidation instead of accumulation.
- **LongMemEval: Benchmarking Chat Assistants on Long-Term Interactive Memory**, arXiv [2410.10813](https://arxiv.org/abs/2410.10813), ICLR 2025. Knowledge update and abstention are measurable skills. → ELO metrics: supersession leaks, abstention.

## Code maps

- **Codebase-Memory: Tree-Sitter-Based Knowledge Graphs for LLM Code Exploration via MCP**, arXiv [2603.27277](https://arxiv.org/abs/2603.27277), Mar 2026 *(summary only)*. Reported: ~10× fewer tokens and 2.1× fewer tool calls than file-by-file exploration, with somewhat lower answer quality (83% vs 92%). → ELO: a map saves cost but must report coverage.
- **LocAgent: Graph-Guided LLM Agents for Code Localization**, arXiv [2503.09089](https://arxiv.org/abs/2503.09089), Mar 2025. Heterogeneous code graphs improve localization.
- **Aider repo map** (tree-sitter + PageRank under a token budget). Centrality is useful for overviews, but must not crowd out task-specific constraints.
- **Language-server references** (TypeScript language service, Deno LSP): exact references through aliases and re-exports. → ELO: optional enrichment (SPEC §7).

## Practice

- Anthropic, *Effective context engineering for AI agents* (2025): progressive disclosure, which means injecting pointers and loading on demand. → ELO pointers (SPEC §4.4).
- Path-scoped rules (`.claude/rules` with `paths`, Cursor rule globs): scope by location. → ELO scopes, generalized to links between projects.

---

*All arXiv IDs must be verified against arxiv.org before a public release. Entries come from searches and model suggestions, and models cite IDs unreliably.*

## Implemented from recent work

- **Delivery, Not Storage: Cue-Anchored Working Memory as a Harness Property for Coding Agents**, arXiv [2607.20972](https://arxiv.org/abs/2607.20972), Jul 2026. → ELO: file-anchored delivery on PreToolUse (`src/cue.rs`).
- **CODESKILL: Learning Self-Evolving Skills for Coding Agents**, arXiv [2605.25430](https://arxiv.org/abs/2605.25430), May 2026. → ELO: recipes (`elo recipe`).
