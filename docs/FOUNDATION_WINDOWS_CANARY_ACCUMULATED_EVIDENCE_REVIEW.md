# Windows Canary Accumulated Evidence Review

## Status

```text
REVIEW_BASE=38e6a0304f8c38b65fe940853079e9fabadb3840
DIAGNOSTIC_BASE=0f3d26fb5a9554def8c74f87dadb5cd2c9a51b22
POST_DIAGNOSTIC_MAIN_RUNS=3
SETUP_FAILURES_OBSERVED=0
WINDOWS_CANARY_REVIEW=PASS
READY_FOR_W3_A=YES
```

This is a read-only evidence review. It changes no Windows code, test timing,
CI workflow, or canary contract.

## Reviewed Population

The diagnostic change landed on `main` in `0f3d26f`. Every `main` push CI from
that commit through the review base was inspected. The population is complete
for that interval:

| Main commit | CI run | Attempt | Windows job | Result |
| --- | ---: | ---: | ---: | --- |
| `0f3d26fb5a9554def8c74f87dadb5cd2c9a51b22` | `34695945639` | 1 | `103559327471` | success |
| `667ef326373d4117e47d76dd584546a8f04158a4` | `34697081670` | 1 | `103562297353` | success |
| `38e6a0304f8c38b65fe940853079e9fabadb3840` | `34711668562` | 1 | `103601425955` | success |

No rerun is included as a substitute for a failed first attempt. All three CI
runs and all three Windows jobs completed successfully on attempt 1.

## Diagnostic Evidence

Both Job Object process-tree canaries emitted both readiness facts in every
reviewed run: the pid file became readable and the process was then observed
alive before cancellation or timeout.

| CI run | Cancellation pid / alive | Timeout pid / alive |
| ---: | --- | --- |
| `34695945639` | 3.257 s / 3.733 s | 3.243 s / 3.733 s |
| `34697081670` | 4.570 s / 5.015 s | 4.566 s / 5.033 s |
| `34711668562` | 3.000 s / 3.355 s | 3.014 s / 3.347 s |

The diagnostic failure strings `grandchild pid never became readable` and
`process-tree (Job) setup failed` do not occur in these Windows logs. Both
`windows_job_cancellation_kills_grandchildren` and
`windows_job_timeout_kills_grandchildren` passed in every sample.

## Conclusion

```text
POST_DIAGNOSTIC_RESULT=NOT_REPRODUCED_IN_3_MAIN_RUNS
WINDOWS_CANARY_DIAGNOSTIC_OBSERVABILITY=PASS
WINDOWS_CANARY_SETUP_ROOT_CAUSE=UNDETERMINED
WINDOWS_CANARY_SETUP_DETERMINISM=NOT_PROVEN
WINDOWS_CANARY_CODE_CHANGED=NO
WINDOWS_CANARY_CI_CHANGED=NO
WINDOWS_CANARY_REVIEW_BLOCKS_W3_A=NO
```

Three successful samples show that the instrumentation is working and that no
new setup failure has accumulated. They do not prove the historical
intermittency is fixed or impossible. A future failure must be evaluated from
its emitted fixture outcome, command, directory state, pid-file state, and
elapsed budget; it must not be converted to a skip or accepted through a rerun.

W3-A may proceed because this read-only gate found no current failure requiring
a Windows repair. The known uncertainty remains recorded rather than silently
closed.
