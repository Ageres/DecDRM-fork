---
name: pi-measure
description: Measure the transmitter's load on a Raspberry Pi 4 with the local station/pi-step0 scripts (step0.sh splits the encoder/OFDM from the resampler and USB with a radio; measure-group.sh runs 1-4 multiplex groups without one), then put the numbers into the docs. Use for "замер на Pi", "step0", "measure-group", "нагрузка", "реальное время", "get_throttled".
---

# Measurements on the Raspberry Pi 4

The Pi 4B (Ubuntu 24.04, 1.8 GHz) has the project at
`/home/bsv/project/github/Ageres/DecDRM-fork`. `station/pi-step0/` is untracked
local material: `step0.sh`, `measure-group.sh`, `README.txt` and the past runs'
logs. Copy the scripts to the Pi; the binary is built there
(`cargo build --release -p decdrm-cli`; the `hackrf` feature is on by default).

## step0.sh — with the radio

Answers "what limits the transmitter: the encoder/OFDM, or the resampler and USB?"

```
sudo STEP0_BIN=/home/bsv/project/github/Ageres/DecDRM-fork/target/release/decdrm bash step0.sh
# and for the real station (3 web streams):
sudo STEP0_BIN=… STEP0_STATION=…/station/station-talk-3.toml bash step0.sh
```

Needs root (the HackRF), no network for the generated tone configs. A run is about
2–4 minutes (longer with perf). Bring back the whole `/tmp/step0-<date>/step0.log`;
its SUMMARY holds the numbers.

- "transmitted N s of signal in M s" with a file output: the encoder/OFDM alone; if
  M ≫ N the resampler is innocent.
- The HackRF runs compare `fast` and `balanced` by exit code, wall time, underruns
  and recoveries.
- The perf section splits rubato (resampler) vs xaac/FDK (encoder) vs IFFT.

## measure-group.sh — no radio, no root

Answers "how much of a core does a group of 1–4 multiplexes take, and is it real
time?"

```
bash measure-group.sh                       # /tmp/measure-group-<date>
MEASURE_BIN=…/target/release/decdrm MEASURE_DUR=30 bash measure-group.sh /path/to/work
```

Six cases, each a `[[multiplex]]` group written to a WAV: 1 carrier; 2 narrow and
3 close (the shared resampler, Fi 48 and 192 kHz); 4 at the 180 kHz limit; 2 and 4
at 300 kHz apart (a stage and a thread each). About 160 MB per case at 20 s, deleted
after each. The end prints a SUMMARY table:

- CPU, % of one core (user+sys over wall time): above 100 % means the wide strategy
  used several cores;
- x real time: below 1.0 is behind real time;
- the plan's strategy line for the case;
- `vcgencmd get_throttled` and the temperature before and after (must stay `0x0`;
  a fan is assumed).

## Where the numbers go

- `docs/design/multi-multiplex.md`: the load table and the predictions note (after
  a real run, replace the predictions and say so).
- `docs/BUILD.md`: the Pi advice, if it changes.
- Keep the measured numbers and the run's date; never fill the table from
  estimates.

## Rules

- Never invent numbers: a table changes only after a real run.
- `measure-group.sh` needs no root; `step0.sh` does (the radio).
- Check `vcgencmd get_throttled` before trusting a slow result.
- Commits with the results only after the user's explicit yes.
