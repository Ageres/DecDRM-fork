# 3. Several multiplexes: status, docs and the Pi 4 measurement

Part of the multi-multiplex feature (see `docs/design/multi-multiplex.md`). Third of
three; depends on the config/plan and the station issues.

Suggested labels: `enhancement`, `docs`, `raspberry-pi`.

## Why

Once the station transmits several multiplexes, the operator has to see them, the
guides have to explain the configuration and the limits, and the 240 % prediction for
four carriers at a 1 MHz span has to be measured on the intended host.

## Scope

- The CLI banner and status line list the multiplexes (frequency, mode, occupancy,
  services); underruns and `recoveries` stay per stream.
- The plan prints the strategy and the thread count.
- The GUI's output card lists the multiplexes; the transmit page's quick fields keep
  applying to the single-multiplex form. Editing several multiplexes in the GUI is a
  separate follow-up.
- Docs: `docs/USER_GUIDE.md` (the new tables and the limits), `docs/BUILD.md` (the
  configuration example and the measured load table), `FORK.md`, `README.md`.
- A Pi 4 measurement script in the spirit of `station/pi-step0`: 1, 2 and 4 multiplexes
  at 9/10 kHz, the narrow group and the 1 MHz span, with a fan and `get_throttled`
  recorded.

## Non-goals

- GUI editing of the multiplexes (a follow-up issue).
- The 26 MHz radio side (the filter, the clock accuracy): documented, not changed.

## Tasks

- [ ] CLI status and banner.
- [ ] The plan's strategy/thread line.
- [ ] GUI output card.
- [ ] Docs and the example station file.
- [ ] The Pi measurement script and the run.
- [ ] The measured numbers in `docs/design/multi-multiplex.md`.

## Acceptance

- A two-multiplex run shows both multiplexes in the status and the GUI.
- The Pi 4 log with 1/2/4 carriers and `get_throttled` is in the design note, with a
  verdict on the four-carrier wide-span case and the fallback.
- The docs describe the configuration and every limit.

## Estimate

2-3 days, plus the Pi run.
