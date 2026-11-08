---
name: live-tx-test
description: Safely run the DecDRM transmitter on this laptop with the attached HackRF One — run --check first, always --duration, the level/crest rules, reading the status (underruns, clipped, recoveries) and stopping. Use when asked to "проверить в эфире", "передать", "прогнать станцию", or when underruns/clipped/recoveries must be interpreted.
---

# A live transmission on this laptop

The HackRF One is attached to this laptop (USB `1D50:6089`), so `decdrm tx` with
`[output.hackrf]` puts **real RF on the air**. Treat every run as a broadcast.

## 1. Build and check the plan

```
$env:CARGO_PROFILE_RELEASE_DEBUG='false'
cargo build --release -p decdrm-cli
.\target\release\decdrm.exe tx <station.toml> --check
```

Read the plan: the radio line (centre, LO, IF, sample rate, TX gain, amplifier), the
occupied band, one line per multiplex (frequency, IF, level) and the span line (the
strategy: "one composite resampler" or "a resampler per multiplex (N threads)").
Fix every problem before transmitting. Station files live in the untracked
`station/`; `station-talk-3.toml` is the user's and is not to be edited.

## 2. Levels: the crest rule

OFDM peaks are about 14 dB above the RMS, so with N multiplexes in one stream
`level_dbfs + 10·log10(N) ≤ −14`:

| carriers | highest level_dbfs |
|---|---|
| 1 | −14 |
| 2 | −17.1 |
| 3 | −18.8 |
| 4 | −20.1 |

The plan rejects a level above the line; −18…−20 dBFS is the practical range.

## 3. Always bound the run

Always pass `--duration SECS`. **Without it a radio run never stops** (the file
output ends with the inputs, the radio output does not). Start with 1 s; 10 s is
plenty for a smoke test. Ask the user before anything longer than a minute.

## 4. While it runs

The status line every `--status-every` seconds (default 10) and at the end:

- `output X dBFS rms, Y dBFS peak, Z clipped` — `clipped > 0` means the levels are
  too high (the crest rule above).
- per multiplex: `… dBFS rms, N frames, SDC a/b bytes`, then one line per service.
- `hackrf: X s transmitted, Y s buffered, underruns, recoveries`:
  - `underruns > 0` — the station cannot feed the radio (too high a rate or too slow
    a machine);
  - `recoveries > 0` — a wedged bulk write was restarted (the station recovers;
    repeated recoveries point at USB);
  - `transmitted` should track wall time for a radio run, far below it for a file.
- `transmitted X s of signal in Y s` — for the radio Y ≈ X (the queue paces it).

A group adds per-multiplex lines after the status line; a spectrum is drawn only for
a single multiplex (a group's stream sits at the radio's frequency).

## 5. Stop and report

Ctrl-C: the stop handle drains the queue and the final status is printed. Never leave
a run unattended, and report the last status line (underruns, clipped, recoveries)
rather than a vague "it worked".

## Notes

- The GUI's Transmitter tab does the same (Start/Stop); the quick frequency field
  edits the first multiplex of a group.
- Wide groups run one stage per multiplex on its own thread: 2–4 carriers fit on
  this laptop; the Raspberry Pi numbers are in the `pi-measure` skill.
- This is a real transmitter: check that the frequency is allowed, prefer a dummy
  load for level tests, and keep the run as short as the test needs.
