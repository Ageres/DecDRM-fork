# 1. Multiplex config and the transmit plan

Part of the multi-multiplex feature (see `docs/design/multi-multiplex.md`). First of
three; it changes no transmitted signal.

Suggested labels: `enhancement`, `hackrf`, `station`.

## Why

One station builds exactly one DRM multiplex. Putting several sources on separate
frequencies (the design target: four, mode B, 9/10 kHz, up to 1 MHz apart) needs the
configuration and the plan to describe them before the signal path can use them.

## Scope

- `MultiplexSettings` (flattened channel fields, services, `level_dbfs`) and
  `StationConfig.multiplexes`; no `[[multiplex]]` keeps today's single multiplex.
- `GroupPlan`: one local oscillator, the offsets, the intermediate rate `Fi`, the
  strategy (shared composite vs per-multiplex resamplers) and the group span.
- Validation: the 1 MHz cap on the outermost carriers, the analog filter edge
  (`HackrfSettings::baseband_filter_hz`, 1.75 MHz at 2 Msps), at least 15 kHz from the
  LO per multiplex, no overlaps, the clip warning, the `fast` occupancy warning per
  multiplex.
- The plan prints the group and one line per multiplex.
- An example station file with 25.76 and 25.78 MHz.

## Non-goals

- Any change to the signal path, the status line or the GUI (the next two issues).
- The file/device output sample rate for composites (decided in issue 2).

## Tasks

- [ ] Config types, serde defaults, backward compatibility.
- [ ] `GroupPlan` and every new validation message.
- [ ] Plan printing, still identical for the single-multiplex form.
- [ ] Tests: 0/1/2/4 multiplexes, each new error, the example.
- [ ] The example station file.

## Acceptance

- `decdrm tx station.toml --check` prints the group line, one line per multiplex and
  the strategy, and rejects more than 1 MHz between the outermost carriers.
- Existing station files validate, and their plan output is unchanged.
- `cargo test --release -p decdrm-station --lib --test config` is green.

## Estimate

1-1.5 days.
