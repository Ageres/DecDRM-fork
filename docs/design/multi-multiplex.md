# Several multiplexes in one HackRF stream

**Status: the configuration, the plan and the combiner are implemented; the
per-multiplex status and the Pi 4 measurement remain.** The design maximum is
**4 multiplexes** (mode B,
9/10 kHz each) spread up to **1 MHz** apart; the first target is 2 (25.76 and
25.78 MHz). A Raspberry Pi 4 is to be measured with 1, 2 and 4 carriers after the
implementation, so the load numbers below are predictions.

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

- **The analog TX filter is the real passband.** The sink sets the HackRF's baseband
  filter to `3/4 x sample_rate_hz` (clamped to 1.75-28 MHz), so at 2 Msps the usable
  band is about +-875 kHz, not +-1 MHz; `plan.rs` checked `sample_rate_hz / 2`, which
  was 25 % too optimistic. The plan uses the same bandwidth as the sink now.
- **The design limit is a separation of at most 1 MHz between the outermost
  carriers.** Inside that limit the span decides the strategy:

  | Separation of the outermost carriers | Strategy | Cost |
  |---|---|---|
  | up to 24 / 48 / 96 kHz | mix at an intermediate rate `Fi` (48, 96 or 192 kHz), then **one** shared resampler `Fi -> sample_rate_hz` | the resampler once (about 55 % of one Pi core with `fast`), independent of N |
  | 96 kHz - 1 MHz | each multiplex resampled 48 kHz -> `sample_rate_hz` on its own, mixed and summed at `sample_rate_hz` | N x the resampler (about N x 55 %), one thread per multiplex |

  The cheap strategy works only while the whole group fits below `Fi / 2`; a 1 MHz
  span needs the composite at 2 Msps, i.e. a resampler per multiplex. On a Pi 4 that
  is comfortable for two carriers and marginal for four (see the load table). A
  polyphase/FFT channelizer would make wide spans cheap, but that is a separate piece
  of work and is out of scope here.
- **Local oscillator.** One LO for the group, `LO = group centre - if_offset_hz` (as
  today, `if_offset_hz` keeps the group off the DC spike and the LO leakage). Each
  multiplex is shifted by `frequency_hz - LO`. With the cheap strategy keep the
  existing -250 kHz IF; a wide group has to straddle the DC, so `if_offset_hz` goes to
  0 and the multiplexes themselves leave a gap around the LO (the DC-spike keep-out is
  per multiplex).
- **Pacing and stop** stay as they are: one clock, one drain, one writer thread.

## Plan and validation

The plan (`decdrm tx --check`) prints the group and one line per multiplex:

```
hackrf: 25.7700 MHz (LO 25.5200 MHz + IF -250 kHz), 2 Msps, TX gain 0 dB, resampler fast
  multiplex 1: 25.7600 MHz, mode B, 10 kHz, IF -260 kHz, services: Programme A
  multiplex 2: 25.7800 MHz, mode B, 10 kHz, IF -240 kHz, services: Programme B
  span: 20 kHz between the carriers (30 kHz of signal), limit 1000 kHz, intermediate rate 48 kHz, one composite resampler
```

It must check, per multiplex and for the group:

- every `frequency_hz` and every shifted channel inside the radio's 1 MHz-6 GHz range;
- every multiplex at least 15 kHz away from the LO (the existing DC-spike rule, now
  per multiplex, not just for the group's centre);
- no two multiplexes overlap (a 10 kHz channel next to another one is fine, on top of
  it is not);
- the separation of the outermost carriers at most 1 MHz (a hard error), and which
  strategy that span selects (printed in the plan line);
- every multiplex inside half the analog filter's bandwidth (3/4 of the sample rate,
  clamped to 1.75-28 MHz) of the LO, with a margin;
- mode/occupancy of each multiplex (as today, per channel);
- the worst case of the resampler: `fast` stays transparent only for the
  4.5-10 kHz occupancies (the existing warning, now per multiplex);
- clipping: the sum of N multiplexes against the 8-bit DAC, with `level_dbfs` per
  multiplex; the OFDM crest factor is about 14 dB (measured), so the plan asks for
  the RMS of the sum to stay that far below full scale and suggests a level.

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

**Cheap strategy, the group below 96 kHz** (e.g. 2 x 10 kHz at 20 kHz spacing, the
intended use): one shared resampler, so N barely matters.

| Multiplexes | One core, % | Real-time margin | Note |
|---|---|---|---|
| 1 | ~60 | 1.6x | measured |
| 2 | ~66 | 1.5x | the intended use |
| 3 | ~72 | 1.4x | |
| 4 | ~78 | 1.3x | about the limit for a comfortable margin |

**Wide spans, 96 kHz - 1 MHz**: a resampler per multiplex, one thread each.

| Multiplexes | One core, % | Threads | Note |
|---|---|---|---|
| 2 | ~120 | 2 | fine on a Pi 4 |
| 3 | ~180 | 3 | tight, leaves one core for everything else |
| 4 | ~240 | 4 | fits the four cores with a fan; the case to measure |

- The 240 % is 2.4 of the four cores. It fits only with one thread per multiplex, a
  fan (2.4 cores sustained on a passive heatsink will throttle at 1.8 GHz) and no
  throttling; the Pi 4 experiment decides. If it falls behind, the fallback is the
  narrow-group strategy (below 96 kHz) or two carriers.
- 18 and 20 kHz multiplexes need `balanced` (about four times the resampler cost),
  about 800 % at four carriers: not on a Pi at all, a desktop task. On the Pi the
  occupancies are 9 and 10 kHz.

- More services per multiplex add about 1-2 percentage points each; four multiplexes
  with three services each add about 10 points.
- The per-multiplex resampler is the price of a span above 96 kHz, not a mistake: the
  cheap path cannot carry a 1 MHz group, because the composite would have to be
  generated at 2 Msps before the mixing.
- `balanced` costs about 200 % already at one multiplex (half of real time) and stays
  unusable on the Pi whatever N is; the Pi needs `fast`.
- RAM: a few MB per multiplex plus the queue; not a concern.

These are predictions; after the implementation, run a `step0`-style measurement with
1, 2 and 4 multiplexes (9/10 kHz: the narrow group and the 1 MHz span) on a Pi 4 with
a fan, record the CPU and `get_throttled`, and put the measured numbers here.

## Work breakdown

1. Config, backward compatibility, validation (including the 1 MHz limit and the
   filter-edge check), the plan - about 1 day.
2. Station: N mux/modulator pipelines, the shared clock, the composite mixer and the
   single resampler; the per-multiplex resampler path for wide spans, with a thread per
   multiplex - about 2.5 days.
3. Status, banner, drain (unchanged) - about half a day.
4. Tests (config, plan, spectrum, smoke) - about 1 day.
5. Docs (USER_GUIDE, BUILD.md with the load table, FORK.md) and an example station file
   - about half a day.
6. Pi 4 measurement with 1/2/4 multiplexes, 9/10 kHz, both strategies, with a fan and
   `get_throttled` recorded - about half a day.
7. GUI: show and edit the multiplexes - 1-2 days, optional, can ship later.

## Risks and decisions

- **Name**: `[[multiplex]]`; `channel` is taken by the DRM channel settings.
- **Backward compatibility**: no `[[multiplex]]` means today's single multiplex; the
  existing station files must work unchanged.
- **LO placement**: one LO for the group; `if_offset_hz` is the group centre. The
  DC-spike check applies to every multiplex.
- **Spacing limit**: the outermost carriers at most 1 MHz apart; the plan rejects more.
  At 2 Msps that is 1 MHz of the 1.75 MHz analog filter, so a wide group has to sit on
  the LO with a gap in the middle rather than at the default -250 kHz IF.
- **Level**: `level_dbfs` per multiplex; the plan warns when the sum clips.
- **Thermal**: four carriers at a 1 MHz span load about 2.4 cores; the experiment
  decides whether a Pi 4 holds it (a fan, `get_throttled`). The fallback is the
  narrow group or two carriers.
- **18 and 20 kHz occupancies** do not scale past one carrier on a Pi: they need
  `balanced`, about four times the resampler.
- **26 MHz radio side** (documentation only): the HackRF is weak and spurious at the
  bottom of its range (a low-pass filter helps, the second harmonic lands at 52 MHz),
  and the stock clock's about 10 ppm is about 260 Hz at 26 MHz - noticeable for DRM.
  The 11 m band is a broadcast allocation, not a free one.
