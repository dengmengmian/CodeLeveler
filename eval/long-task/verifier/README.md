# Long-task verifiers

Independent of the agent. They live in the private control repo so the model
cannot read them from the product tree.

```
$CONTROL_ROOT/ma-wa1-long-task-orchestration-utility/scripts/verify-long-{a,b,c}.sh
```

`eval/scripts` never inlines those checks. A batch record stores
`verifier.passed` after the script runs, or `verifier.ran=false` when
`CONTROL_ROOT` is unset.
