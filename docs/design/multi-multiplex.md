# Several multiplexes in one HackRF stream

**Status: draft, not implemented.** Target: 2 DRM multiplexes on 25.76 and 25.78 MHz
(mode B, 10 kHz each); the design should scale to 4.

## Goal

One station process transmits **N independent DRM multiplexes** through **one** HackRF
One. A multiplex is what the DRM standard puts on one frequency: its own robustness
mode, occupancy, protection and services. Every multiplex keeps its own service set,
its own frequency and its own level; they share the radio: one sample rate, one TX
gain, the amplifier, `buffer_s`, the drain and the underrun/`recoveries` counters.

The radio is not the constraint: HackRF One takes 2-20 Msps (2 Msps is the minimum) and
tunes 1 MHz-6 GHz, so two 10 kHz channels 20 kHz apart (a 30 kHz composite) use a
sixtieth of a 2 Msps stream. The constraint is the station, which today has exactly one
multiplex.

## Non-goals

- Several HackRF devices. One stream, one radio; a second radio means a second station.
- Transmitting one service on two frequencies (single-frequency networks, SFN). Not a
  feature of the fork today; out of scope.
- Editing the multiplexes in the GUI. The first release can show them; editing can
  come later.
- Anything about the 26 MHz radio side (a low-pass filter, the clock's accuracy). Worth
  documenting, but it does not change the code.

## Terminology

The config already uses `[channel]` for the DRM channel's parameters (`mode`,
`occupancy`, the interleaving and the protection). The new level is therefore
**`multiplex`**: a `[channel]` plus a frequency plus its services.

## Configuration

The single-multiplex form stays as it is: with no `[[multiplex]]` table, the station is
what it is today, one multiplex built from `[channel]`, `[[service]]` and
`[output.hackrf] frequency_hz`.

```toml
[output.hackrf]
sample_rate_hz = 2_000_000
resampler = "fast"
# The centre of the whole group relative to the local oscillator, Hz.
if_offset_hz = -250_000
buffer_s = 1.0
tx_gain_db = 40
amp_enable = false

[[multiplex]]
frequency_hz = 25_760_000
mode = "B"
occupancy = 10
level_dbfs = -12           # per multiplex: the sum of four must not clip the 8-bit DAC

[[multiplex.service]]
label = "Programme A"
id = 1
# ... as today

[[multiplex]]
frequency_hz = 25_780_000
mode = "B"
occupancy = 10
level_dbfs = -12

[[multiplex.service]]
label = "Programme B"
id = 1                     # unique within its own multiplex, not across the station
```

Each multiplex gets its own `[channel]`-style fields (mode, occupancy, MSC/SDC mode,
interleaving, protection) and its own `[[multiplex.service]]`. IDs are per multiplex:
they travel in that multiplex's FAC/MSC and mean nothing to its neighbours.

## Signal path

```
multiplex 1: services -> MSC frame -> DRM modulator (48 kHz) -> upsample to Fi
                                                                  -> NCO shift -> sum
multiplex 2: services -> MSC frame -> DRM modulator (48 kHz) -> upsample to Fi
                                                                  -> NCO shift -> sum
                                                                        |
                                        one resampler Fi -> sample_rate_hz
                                                                        |
                                                           HackrfSink (drain, recoveries)
```

- **One resampler, not one per multiplex.** The 48 kHz -> 2 Msps resampler is the
  dominant cost (about 55 % of one core on a Pi 4B with `fast`); it works on the sum,
  so N does not multiply it.
- **Intermediate rate `Fi`** is the smallest of 48, 96 and 192 kHz that covers the
  group's span (twice the highest offset plus a margin):

  | Group span | `Fi` |
  |---|---|
  | up to 24 kHz (e.g. 2 x 10 kHz at 10 kHz spacing) | 48 kHz |
  | up to 48 kHz (2 x 10 kHz at 20 kHz spacing) | 96 kHz |
  | up to 96 kHz (4 x 10 kHz at 20 kHz spacing -> 70 kHz) | 192 kHz |

- **Local oscillator.** One LO for the group, `LO = group centre - if_offset_hz` (as
  today, `if_offset_hz` keeps the group off the DC spike and the LO leakage). Each
  multiplex is shifted by `frequency_hz - LO`. The 2 Msps passband (about +-1 MHz)
  gives plenty of room; the constraint is the DC spike, not the edges.
- **Pacing and stop** stay as they are: one clock, one drain, one writer thread.

## Plan and validation

The plan (`decdrm tx --check`) prints the group and one line per multiplex:

```
hackrf: 25.7700 MHz (LO 25.5200 MHz + IF -250 kHz), 2 Msps, TX gain 0 dB, resampler fast
  multiplex 1: 25.7600 MHz, mode B, 10 kHz, IF -260 kHz, services: Programme A
  multiplex 2: 25.7800 MHz, mode B, 10 kHz, IF -240 kHz, services: Programme B
  span: 30 kHz of a 2000 kHz passband, intermediate rate 96 kHz
```

It must check, per multiplex and for the group:

- every `frequency_hz` and every shifted channel inside the radio's 1 MHz-6 GHz range;
- every multiplex at least 15 kHz away from the LO (the existing DC-spike rule, now
  per multiplex, not just for the group's centre);
- no two multiplexes overlap (a 10 kHz channel next to another one is fine, on top of
  it is not);
- the whole group inside the sample rate with a margin;
- mode/occupancy of each multiplex (as today, per channel);
- the worst case of the resampler: `fast` stays transparent only for the
  4.5-10 kHz occupancies (the existing warning, now per multiplex);
- clipping: the sum of N multiplexes against the 8-bit DAC, with `level_dbfs` per
  multiplex (default lowered by the plan when N is large, or a warning).

## Status

- The CLI banner lists the multiplexes (frequency, mode, occupancy, services).
- The status line carries the transmitted seconds once for the stream; underruns and
  `recoveries` are per stream.
- The GUI's Output card can list the multiplexes read-only; the transmit page's quick
  fields keep working for the single-multiplex form.

## Tests

- Config: no `[[multiplex]]` (today's form), one, two, four; per-multiplex services and
  IDs; unknown fields rejected; a multiplex without services rejected.
- Plan: overlap, the LO distance per multiplex, the span against the sample rate, the
  intermediate rate choice, the `fast` warning per multiplex, the clip warning.
- Signal: generate two multiplexes to a file and check the spectrum at both offsets
  (FFT peaks at the requested frequencies, level within the expected range).
- Regression: the single-multiplex output is bit-identical to the current code for the
  same config (the new path must not change the existing one).

## Load on a Raspberry Pi 4B

Extrapolated from the measured single-multiplex numbers (`fast`, 2 Msps, 1.8 GHz, no
throttling): the resampler is about 55 % of one core, a multiplex with 1-3 services
about 3-5 %, the mixing and the interpolation about 1-3 % per multiplex.

| Multiplexes | One core, % | Real-time margin | Note |
|---|---|---|---|
| 1 | ~60 | 1.6x | measured |
| 2 | ~66 | 1.5x | the intended use |
| 3 | ~72 | 1.4x | |
| 4 | ~78 | 1.3x | about the limit for a comfortable margin |

- More services per multiplex add about 1-2 percentage points each; four multiplexes
  with three services each add about 10 points.
- **One resampler per multiplex instead of one shared** would cost N x 55 %: 110 % at
  two, 220 % at four. That is the mistake to avoid.
- `balanced` costs about 200 % already at one multiplex (half of real time) and stays
  unusable on the Pi whatever N is; the Pi needs `fast`.
- RAM: a few MB per multiplex plus the queue; not a concern.

These are predictions; after the implementation, run a `step0`-style measurement with
1, 2 and 4 multiplexes and put the measured numbers here.

## Work breakdown

1. Config, backward compatibility, validation, the plan - about 1 day.
2. Station: N mux/modulator pipelines, the shared clock, the composite mixer and the
   single resampler - about 2 days.
3. Status, banner, drain (unchanged) - about half a day.
4. Tests (config, plan, spectrum, smoke) - about 1 day.
5. Docs (USER_GUIDE, BUILD.md with the load table, FORK.md) and an example station file
   - about half a day.
6. Pi 4 measurement with 1/2/4 multiplexes - about half a day.
7. GUI: show and edit the multiplexes - 1-2 days, optional, can ship later.

## Risks and decisions

- **Name**: `[[multiplex]]`; `channel` is taken by the DRM channel settings.
- **Backward compatibility**: no `[[multiplex]]` means today's single multiplex; the
  existing station files must work unchanged.
- **LO placement**: one LO for the group; `if_offset_hz` is the group centre. The
  DC-spike check applies to every multiplex.
- **Level**: `level_dbfs` per multiplex; the plan warns when the sum clips.
- **26 MHz radio side** (documentation only): the HackRF is weak and spurious at the
  bottom of its range (a low-pass filter helps, the second harmonic lands at 52 MHz),
  and the stock clock's about 10 ppm is about 260 Hz at 26 MHz - noticeable for DRM.
  The 11 m band is a broadcast allocation, not a free one.
