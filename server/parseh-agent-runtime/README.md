# parseh-agent-runtime

The **pillar-1 executable primitive**: an executor that turns a signed
`AgentDefinition` (from `parseh-agent-spec`) into actual LLM-backed
work, plus a minimal workflow chainer — the n8n-analogue.

See the project notes §"Pillar 1 — Free
distributed AI for builders". `parseh-agent-spec` is the *schema*;
this crate is the *runtime* that executes that schema. Before this
crate, nothing in PARSEH executed an agent definition (per
the project notes
§"Free AI access").

## What it does

```text
AgentDefinition + JobSpec + input JSON
  ├── validate input JSON against AgentDefinition.input_schema
  ├── resolve knowledge refs from the local content-addressed cache
  ├── render prompt_template with input + resolved knowledge
  ├── backend.complete(prompt, seed, max_tokens)   [seed from JobSpec]
  ├── parse + validate output against AgentDefinition.output_schema
  └── build + sign a JobResult (parseh-task)
```

Plus `Workflow`: a DAG of agent steps with explicit output→input
wiring, executed in topological order, cycles rejected before any
agent runs. This is the "build an n8n-style automation graph" piece —
**locally** for now.

## The honest caveat (binding)

This runtime executes agents **locally**, against:

- a **local Ollama** daemon (`OllamaBackend`, endpoint discovered via
  `parseh-llm-detect`), or
- the deterministic **`MockBackend`** (tests; no network).

The FREE DISTRIBUTED execution that pillar 1 promises — a builder
priced out of commercial APIs submits a workflow and the *volunteer
network* runs it for free, with peers verifying the result — requires
the network to exist. That needs **bootstrap servers, which are not
provisioned**. This crate is the executable primitive for pillar 1; it
is **not** the operational free-AI service. Running an agent here costs
*you* local compute. Nothing in this crate reaches the volunteer
network.

There is **zero economic / payment / reward logic** and **zero
marketplace / agent-discovery** code here, by design — both are
deferred per
the project notes
and the audits' refusal of "tradeable instructions". This is a
scaffold-grade primitive, not production-ready.

## Deterministic-mode rationale

Execution is **deterministic-mode only** (seed-pinned, Ollama
`temperature: 0`). The seed comes from the `JobSpec`; `executed_at` is
anchored to `JobSpec.submitted_at` (not wall-clock). The consequence:
the same agent + same input + same seed + **same model** produces a
**byte-identical signed `JobResult`**. This is precisely the property
`parseh-verify` relies on to re-execute a result and counter-sign it.

Caveat recorded honestly: Ollama's seed determinism is *model- and
runtime-version-dependent*. A pinned seed reproduces output only on the
**same model weights and the same Ollama build** — the seed alone is
not sufficient. `JobResult.result_meta.model_used` carries the model id
so a verifier can refuse a model mismatch rather than emit a false
disagreement. Non-deterministic / cross-model verification is V0.3+.

## Deferred to V0.3+

- Network fetch of knowledge corpora. `TextCorpus` knowledge refs are
  resolved only from the local content-addressed cache
  (`~/.parseh/knowledge/<sha256-hex>`, hash-checked). A missing corpus
  is a clear typed error, not a silent network fetch — a fetch path
  that pulled different bytes under the same hash would be a
  verification hole.
- `EmbeddingIndex`, `StructuredDataset`, and ref-style
  `UpstreamAgentOutput` knowledge kinds — typed `*Unsupported` errors.
  (Agent composition is available *now* via `Workflow`, which wires
  step outputs explicitly.)
- Non-deterministic / spot-check / statistical verification.
- Distributed execution across the volunteer network (needs
  bootstrap).
- Anything economic: rewards, quality scoring, marketplace, discovery.

## Tests

`cargo test -p parseh-agent-runtime` — 13 unit + 18 integration tests.
Covers schema-validated I/O, template rendering, mock determinism, the
full signed-`JobResult` pipeline, knowledge cache resolution + hash
mismatch, linear/diamond/cyclic workflows, a wiremock Ollama backend
(asserts `seed` + `temperature: 0` are sent), and the byte-identical
`JobResult`-across-two-runs determinism guarantee.
