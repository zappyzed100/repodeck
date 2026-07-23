# Real-machine placement soak test

Drives a running RepoDeck through hundreds of random workset switches per
config size and verifies every window actually lands where RepoDeck's log says
it placed it (drift detection), that overflow windows minimize, that the target
is (near-)full on a main monitor, and that no two visible managed windows
overlap. A first-pass mismatch is re-measured after ~1.8s and only counted if it
*persists* (real drift), so windows still settling aren't false positives.

## Pieces
- `repodeck-simtest` (`src/bin/repodeck-simtest.rs`) — the validator: connects to
  RepoDeck's env-gated test-control pipe, commands `switch <uuid>`, parses the
  log for intended placements, and checks the live windows. Built with the
  normal `cargo build --release`.
- `spawn-windows.ps1 -Sets N` — spawns 2N labelled WinForms windows `SET-01-A`..`SET-0N-B`.
- `kill-setwins.ps1` — kills every process owning a `SET-*` window (cleanup).
- `slice_config.py <config.json> N` — keeps the first N worksets of a config.
- `run-soak.ps1 [-Iters 300] [-SettleMs 1200]` — orchestrates sizes 3/6/9/12/15:
  stop RepoDeck, slice config, respawn windows, start RepoDeck with
  `REPODECK_TEST_CONTROL=1` (skips UAC), run the validator, collect PASS/FAIL.

## Prereqs
- A `_master_config.json` (a full 15-set config with live monitor bounds) next
  to `run-soak.ps1`, used as the slicing template.
- Windows titled `SET-XX-Y` are matched by the worksets' `title_contains`.

## Run
```powershell
pwsh -NoProfile -File scripts/soak/run-soak.ps1 -Iters 300 -SettleMs 1200
```
Results land in `scripts/soak/soak-results/result_<N>.json`.
