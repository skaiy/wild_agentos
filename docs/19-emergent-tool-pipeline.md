# Emergent Tool Promotion Pipeline

Generated tools are treated as untrusted data until a person promotes them through each gated stage:

```text
proposed --(sandbox/judge gate + human approval)--> session_enabled
session_enabled --(tenant gate + human approval)--> tenant_candidate
tenant_candidate --(publish gate + human approval)--> published
```

A human reviewer may move a non-terminal record to `rejected`; rejected and published records are terminal. There is deliberately no API that promotes a proposal directly to `published`.

## Safety and isolation

- The kernel does not execute generated code. An `EmergentToolGate` adapter evaluates it, typically by delegating to an external isolated sandbox and judge.
- A failed or unavailable gate leaves the record in its current state.
- Every promotion stores its gate verdict, evaluator, named approver, and timestamp.
- The `session_enabled` → `tenant_candidate` transition additionally requires a
  completed, named rule-review checklist for safety, tenant isolation, and
  declared side effects. The audit record binds that review and gate verdict to
  a SHA-256 digest of the reviewed definition.
- `EmergentToolStore` requires a verified tenant identifier and is intended to use an L0 store opened with verified tenant claims. It also verifies the record tenant before reads and mutations, so another tenant cannot enumerate or promote it.
- Proposing stores only a draft record. It does not write to the skill graph, registry, or any production tool surface. Publication integration must consume only records in `published`.

## Integration contract

Implement `EmergentToolGate` for the sandbox/test/judge service available in the deployment. Its `evaluate` method is invoked immediately before each requested transition. Return a passing verdict only after the relevant sandbox, test, policy, and judge checks have completed.

## Graph Engineering cross-cut / 图工程跨切

Tenant promotion is a supervisory loop, not an optimizer outcome: frozen
golden input/output fixtures are pinned by package-declared SHA-256 digests and
are verified before evaluation; a changed fixture is rejected rather than
scored. The independent rule-review loop and its named human reviewer govern
safety, isolation, and side-effect checks before tenant visibility. Promotion
audit fields preserve the candidate digest, gate evidence, reviewer, and time.

This is “loops watching loops”: a generator may optimize a candidate, but it
cannot rewrite its scorecard or grant tenant authority to itself.
