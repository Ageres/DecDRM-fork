# 2. The station: several multiplexes in one stream

Part of the multi-multiplex feature (see `docs/design/multi-multiplex.md`). Second of
three; depends on the config and plan issue.

Suggested labels: `enhancement`, `hackrf`, `station`.

## Why

The station holds exactly one multiplex today: one `MultiplexPlan`, one `Transmitter`,
one set of audio/data/Journaline chains, one `HackrfSink` that also resamples. Two to
four multiplexes have to be built, shifted to their offsets and summed into one stream.

## Scope

- Split `Station` into `Multiplex` (plan, transmitter, services, status) and `Station`
  (the group, the clock, the output stage, the sinks). The single-multiplex output must
  stay byte-identical.
- A combiner: an NCO shift per multiplex, then
  - spans up to 96 kHz: the composite at an intermediate rate `Fi` (48/96/192 kHz) and
    **one** shared resampler `Fi -> sample rate`;
  - spans 96 kHz - 1 MHz: a resampler per multiplex (`48 kHz -> sample rate`), the sum
    at the sample rate, one thread per multiplex.
- `HackrfSink` stops resampling: the combiner feeds it the final-rate samples. The
  drain, the underruns and the write recovery stay as they are.
- `output.sample_rate_hz` for the file and device outputs (the default 48 kHz), needed
  to write a composite to a file for the tests.
- Per-multiplex `[mdi]`, `[simulate]`, `[afs]`, feeds and web streams.
- Tests: the spectrum of a two-multiplex file at both offsets; a byte-identical
  regression for one multiplex; a stop during a multi-multiplex run.

## Non-goals

- Status and GUI (the next issue).
- The polyphase/FFT channelizer that would make wide spans cheap.

## Tasks

- [ ] `Multiplex`/`Station` split, single-multiplex regression hash.
- [ ] The combiner and the intermediate-rate choice.
- [ ] Wide spans: per-multiplex resamplers and threads.
- [ ] `HackrfSink` without its resampler.
- [ ] `output.sample_rate_hz`.
- [ ] Tests (config, spectrum, regression, stop).

## Acceptance

- Two multiplexes on 25.76 and 25.78 MHz in one file at 2 Msps show FFT peaks at both
  offsets, with no clipping.
- The single-multiplex file output is byte-identical to `v1.0.0`.
- A two-multiplex, 1 MHz-span run keeps real time on a desktop.
- Station and GUI tests, clippy and `--no-default-features` are green.

## Estimate

4-5 days.
