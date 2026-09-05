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
- `EmergentToolStore` requires a verified tenant identifier and is intended to use an L0 store opened with verified tenant claims. It also verifies the record tenant before reads and mutations, so another tenant cannot enumerate or promote it.
- Proposing stores only a draft record. It does not write to the skill graph, registry, or any production tool surface. Publication integration must consume only records in `published`.

## Integration contract

Implement `EmergentToolGate` for the sandbox/test/judge service available in the deployment. Its `evaluate` method is invoked immediately before each requested transition. Return a passing verdict only after the relevant sandbox, test, policy, and judge checks have completed.
