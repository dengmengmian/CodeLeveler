# Ownership

Cases stay in `$CONTROL_ROOT/ownership-final/cases`. After a slot finishes:

```sh
python3 eval/scripts/score_eventlog.py "$LEVELER_HOME"
```

A PASS on overlap is `ownership_denied > 0` plus the child's own write still
succeeding. See `ownership-final/SAFETY_MATRIX.md` in the control plane.
