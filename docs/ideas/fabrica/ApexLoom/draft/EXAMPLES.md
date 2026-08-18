# Apex Loom — Example Machines (v0.1)

These are ready-to-adapt fixtures for the pure engine and for grok-build tests.

## 1. Writer-Critic (classic feedback loop)

```yaml
spec: loom
spec_version: "0.1.0"
data:
  name: writer-critic
  description: "Write → critique → revise until quality threshold or max iterations"
  context:
    topic: "{{ input.topic }}"
    draft: ""
    critique: ""
    iterations: 0
  settings:
    max_steps: 12
  states:
    start:
      type: initial
      transitions:
        - to: write
    write:
      action: prompt
      input:
        text: |
          Write a clear, complete draft on the topic: {{ context.topic }}.
          Previous critique (if any): {{ context.critique }}
      output_to_context:
        draft: "{{ output }}"
      transitions:
        - to: critique
    critique:
      action: prompt
      input:
        text: |
          Critique the following draft for accuracy, completeness, clarity, and style.
          Give an overall score from 1-10 and list concrete issues.
          Draft:
          {{ context.draft }}
      output_to_context:
        critique: "{{ output }}"
        iterations: "{{ context.iterations + 1 }}"
      transitions:
        - condition: "context.iterations >= 3 or contains(context.critique, 'score: 8') or contains(context.critique, 'score: 9') or contains(context.critique, 'score: 10')"
          to: done
        - to: write
    done:
      type: final
      output:
        final_draft: "{{ context.draft }}"
        iterations: "{{ context.iterations }}"
        last_critique: "{{ context.critique }}"
```

## 2. Research Loop (gather → synthesize)

```yaml
spec: loom
spec_version: "0.1.0"
data:
  name: research-loop
  description: "Simple research → draft → critique cycle"
  context:
    query: "{{ input.query }}"
    notes: ""
    draft: ""
    critique: ""
    score: 0
  settings:
    max_steps: 20
  states:
    start:
      type: initial
      transitions:
        - to: research
    research:
      action: prompt
      input:
        text: "Research the topic thoroughly: {{ context.query }}. Produce structured notes with sources if possible."
      output_to_context:
        notes: "{{ output }}"
      transitions:
        - to: draft
    draft:
      action: prompt
      input:
        text: "Using these notes, write a concise, well-structured draft.\n\nNotes:\n{{ context.notes }}"
      output_to_context:
        draft: "{{ output }}"
      execution:
        type: retry
        max_attempts: 3
        backoffs: [2, 8, 16]
      transitions:
        - to: critique
    critique:
      action: prompt
      input:
        text: |
          Critique this draft for accuracy and clarity. End with a line "SCORE: N" where N is 0-10.
          Draft:
          {{ context.draft }}
      output_to_context:
        critique: "{{ output }}"
      transitions:
        - condition: "contains(context.critique, 'SCORE: 8') or contains(context.critique, 'SCORE: 9') or contains(context.critique, 'SCORE: 10')"
          to: done
        - to: revise
    revise:
      action: prompt
      input:
        text: "Revise the draft based on this critique:\n{{ context.critique }}\n\nOriginal draft:\n{{ context.draft }}"
      output_to_context:
        draft: "{{ output }}"
      transitions:
        - to: critique
    done:
      type: final
      output:
        result: "{{ context.draft }}"
        critique: "{{ context.critique }}"
```

## 3. Coding Pipeline (plan → fanout implement → integrate)

Demonstrates mapping to existing `task_fanout` + evidence paths.

```yaml
spec: loom
spec_version: "0.1.0"
data:
  name: coding-pipeline
  description: "Plan a feature, fan out implementation work, integrate and verify"
  context:
    feature: "{{ input.feature }}"
    plan: ""
    artifacts: []
  settings:
    max_steps: 30
  states:
    start:
      type: initial
      transitions:
        - to: plan
    plan:
      action: prompt
      input:
        text: |
          Produce a detailed, discrete implementation plan for the feature:
          {{ context.feature }}
          List clear, independent tasks that can be parallelized.
      output_to_context:
        plan: "{{ output }}"
      transitions:
        - to: implement
    implement:
      action: fanout
      input:
        # In real use the engine or a hook expands the plan into concrete task prompts.
        # For the fixture we keep it simple / illustrative.
        tasks:
          - "Implement the core logic for: {{ context.feature }} according to the plan."
          - "Write tests for: {{ context.feature }} according to the plan."
          - "Handle edge cases and error paths for: {{ context.feature }}."
        mode: async
        batch_deadline_s: 1800
      wait_for: batch_done
      output_to_context:
        artifacts: "{{ batch.evidence_paths }}"
      transitions:
        - to: integrate
    integrate:
      action: prompt
      input:
        text: |
          Read the evidence artifacts at these paths: {{ context.artifacts }}.
          Integrate the changes, run verification, and either confirm success or list remaining work.
          Original plan:
          {{ context.plan }}
      transitions:
        - condition: "contains(output, 'verified') or contains(output, 'tests passed')"
          to: done
        - to: implement   # re-fan for fixes if needed
    done:
      type: final
      output:
        result: "{{ context.artifacts }}"
        plan: "{{ context.plan }}"
```

## Notes for implementers

- Templates use a deliberately limited `{{ path }}` and simple helpers (`contains`, arithmetic on numbers). Complex logic belongs in the prompt or in a Rust hook.
- `wait_for: batch_done` is resolved by the LoomDriver when it receives the corresponding `TaskBatchDone` event for the instance’s batch.
- Fanout tasks that need dynamic expansion from `context.plan` can be handled by a small registered hook (`action: hook, name: expand_plan_tasks`) rather than making the YAML expression language Turing-complete.
- All three examples are self-contained fixtures for unit tests of the pure engine (feed mock `output` / `batch` results and assert transitions + final context).
