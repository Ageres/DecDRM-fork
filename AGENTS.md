# AGENTS.md — DecDRM-fork (Ageres)

Fork of `CasualArclamp/DecDRM`: a Rust DRM30 receiver and transmitter. Read
`docs/DESIGN.md` for the architecture; this file is the working agreement for this
fork. `reference/` (Dream r1548 and the DRM spec as text, `es_201980v040201p.txt`) is
local and untracked: grep it for clause numbers and tables.

## Crates

- `crates/decdrm-core` — the DRM30 PHY/FEC/multiplex, the receiver and transmitter
  chains, the channel simulator, DSP (pure Rust, `f64`).
- `crates/decdrm-codecs` (+ `-fdk-sys`, `-opus-sys`, `-xaac-sys`) — FDK-AAC, libopus
  and libxaac (xHE-AAC) FFI: the encoders behind the audio services.
- `crates/decdrm-io` — WAV/FLAC, resampling (rubato), sound card (cpal).
- `crates/decdrm-data` — packet mode, MOT, Journaline, EPG, BWS, TPEG.
- `crates/decdrm-engine` — the receiver's worker thread and session: inputs (file,
  sound card, KiwiSDR, MDI) to audio, text and data.
- `crates/decdrm-station` — the transmitter application layer: the TOML station
  config, the plans, `Station` and `GroupStation` (this fork's feature), feeds.
- `crates/decdrm-schedule` — DRM broadcast schedules (Dream's `DRMSchedule.ini`,
  EiBi CSV).
- `crates/decdrm-mdi` — MDI (TS 102 820) and RSCI/RCI over DCP, recordings.
- `crates/decdrm-kiwi` — the KiwiSDR client and a mock for tests.
- `crates/decdrm-evs` — EVS signalling for recognition only: never add 3GPP/EVS
  reference code (copyright, patents).
- `crates/decdrm-dac` — the DAC neural codec extension; its weights live in the
  untracked `models/`, `candle` only behind the `dac` feature.
- `apps/decdrm-cli` (binary `decdrm`) and `apps/decdrm-gui` (egui).
- `third_party/` — pinned submodules (fdk-aac, opus, libxaac): clone with
  `--recursive`.

## Git

- `origin` is `https://github.com/Ageres/DecDRM-fork.git`; work goes on `dev-03`
  (the fork's own version line, see `FORK.md`: 1.1.0; the last tag `v1.0.0` is on
  `dev-02`).
- **Never commit without an explicit "yes" from the user.** Show the proposed message
  first and wait; the user pushes. No amend, no force-push, no history rewriting.
- Author: `Ageres <Ageres@users.noreply.github.com>`.
- Message style: a summary line, then what changed and why, wrapped at ~80 columns.
- PowerShell writes commit messages as UTF-8 only through a file:
  `[IO.File]::WriteAllText($path, $msg, (New-Object Text.UTF8Encoding($false)))`,
  then `git commit -F $path`. In PowerShell strings a backtick is an escape and a
  double-quoted here-string keeps `""` as two quotes — prefer single-quoted
  here-strings (`@' … '@`). CRLF warnings on commit are harmless: `.gitattributes`
  normalizes everything to LF.

## Building and checking (Windows)

- `$env:CARGO_PROFILE_RELEASE_DEBUG='false'` before `cargo test`/`cargo clippy`
  (release without debug info).
- **Do not run `cargo fmt`**: the tree is not rustfmt-clean; match the local style by
  hand (rustfmt diffs are expected and not a problem to fix).
- The checks used for the transmitter work:
  - `cargo test --release -p decdrm-station` (lib and integration tests)
  - `cargo test --release -p decdrm-gui`
  - `cargo test --release -p decdrm-cli`
  - `cargo clippy --release -p decdrm-station -p decdrm-cli -p decdrm-gui --all-targets -- -D clippy::correctness`
  - `cargo check -p decdrm-station --no-default-features --all-targets` (a group
    that only writes a file must build without the `hackrf` feature)
- Single-multiplex regression (the group path must not change the single path). The
  signal carries the SDC time (type 8) from the clock, so two runs a minute apart
  differ; pin it first: copy the example and `journaline.toml` to a temp dir and add
  `start = "2026-09-29T18:00:00Z"` right under `[time]`
  (`sed '/^\[time\]/a start = "2026-09-29T18:00:00Z"' station.toml > pinned.toml`).
  Then `decdrm tx pinned.toml --duration 10 --output a.wav` is deterministic: the
  same command and tree give the same `sha256` before and after a change. (The value
  is not fixed across toolchains or platforms; compare within a session.)
- Bash tasks (scripts, `bash -n`) run through WSL:
  `wsl -d Ubuntu -- bash -lc "cd /mnt/c/a/project/github/Ageres/DecDRM-fork && bash scripts/…"`.

## Local, untracked material

Never commit and never "clean up": `station/` (personal station files, including
`station-talk-31.toml`), `station/pi-step0/` (the Pi measurement scripts and past
logs), `samples/`, `reference/`, `models/`, `out/`, `exe/`. The Pi project is at
`/home/bsv/project/github/Ageres/DecDRM-fork`. The repository is public: no
recordings, reference material, model weights, secrets, third-party material or
3GPP/EVS reference code.

## Hardware

The HackRF One is attached to this laptop (USB `1D50:6089`), so `decdrm tx` with
`[output.hackrf]` puts real RF on the air: always `--check` first and always give
`--duration` (without it the run never stops). Levels follow the crest rule: OFDM
peaks are about 14 dB above the RMS, so `level_dbfs + 10·log10(N) ≤ −14` for N
multiplexes (−18…−20 dBFS in practice). See the `live-tx-test` skill.

## The multi-multiplex feature (the fork's main addition)

Design and history: `docs/design/multi-multiplex.md` and `docs/design/issues/01…03`.
`[[multiplex]]` tables replace the top-level `[channel]` and `[[service]]`; limits:
up to 4 carriers, at most 1 MHz of span, at least 15 kHz from the local oscillator,
inside the analog filter (`3/4 × sample_rate_hz`, ±875 kHz at 2 Msps). The stream's
settings live in `[output]` (`sample_rate_hz`, `resampler`) when a group writes a
file; `[output.hackrf]` keeps the radio's (`tx_gain_db`, `amp_enable`, `if_offset_hz`,
and its `sample_rate_hz` must agree with `[output]`). Up to about 180 kHz of span the
multiplexes are mixed at an intermediate rate (48, 96 or 192 kHz) and one shared
resampler brings the sum to the radio's rate; wider groups resample each multiplex on
its own thread, one per multiplex per frame (`Combiner::push_frame`). `GroupStation`
either drives the radio or writes one I/Q WAV/FLAC; `decdrm tx --output FILE` records
what a radio configuration would send.

## Where things are

- `crates/decdrm-station/src/{config,plan}.rs` — `[[multiplex]]` parsing and
  `StationPlan` (`GroupPlan`, strategies, validation).
- `crates/decdrm-station/src/{combine,group}.rs` — the combiner (NCO phasor,
  resamplers, threads) and `GroupStation` (stations, sink, status).
- `apps/decdrm-cli/src/tx.rs` — `Running::{Single,Group}`, the status lines,
  `--output`.
- `apps/decdrm-gui/src/{tx_config,transmitter}.rs`, `panels/tx_page.rs` — the GUI.
- `docs/USER_GUIDE.md` (config reference and the group section), `docs/BUILD.md`
  (Pi build and load), `docs/FORK.md` (fork changes and version line),
  `scripts/pi/stationctl` (running a station on the Pi, tracked).

## Skills

- `live-tx-test` — a safe test transmission on this laptop's HackRF.
- `pi-measure` — the load measurements on the Raspberry Pi 4 (`station/pi-step0`).
