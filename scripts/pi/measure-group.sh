#!/usr/bin/env bash
# The load of a multi-multiplex group: 1-4 carriers, narrow and wide, on a Raspberry
# Pi 4 (any Linux works; `vcgencmd` lines are skipped without it).
#
#     bash scripts/pi/measure-group.sh                 # work dir: /tmp/measure-group-<date>
#     bash scripts/pi/measure-group.sh /path/to/work
#     MEASURE_BIN=/path/to/target/release/decdrm bash scripts/pi/measure-group.sh
#     MEASURE_DUR=30 MEASURE_RATE=2000000 bash scripts/pi/measure-group.sh
#
# It needs no radio and no network: every case writes its composite to a WAV in the
# work dir (removed after the case), so what it measures is the station itself: the
# encoders, the modulators, the combiners and their resamplers. The numbers are the
# ones docs/design/multi-multiplex.md's load table wants:
#
#   - CPU, % of one core (user+sys over the wall time): above 100 % means the
#     per-multiplex stages of a wide group ran on several cores;
#   - the wall time against the signal's duration (x real time): under 1.0 is not
#     real time;
#   - `vcgencmd get_throttled` and the temperature before and after.
#
# The cases: 1 carrier; 2 and 3 close together (the shared resampler, Fi 48 and 192
# kHz); 4 near the 180 kHz limit; and 2 and 4 carriers 300 kHz apart (a resampler per
# multiplex, a thread each). Copy the SUMMARY into the doc's table.

set -u
# A UTF-8 terminal on the Pi:
export LC_ALL=
WORK=${1:-/tmp/measure-group-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$WORK" || exit 1
LOG="$WORK/measure.log"
: >"$LOG"
exec > >(tee -a "$LOG") 2>&1

DUR=${MEASURE_DUR:-20}
RATE=${MEASURE_RATE:-2000000}
FIRST_HZ=25760000
ROWS="$WORK/summary.tsv"
: >"$ROWS"

say() { echo; echo "===== $* ====="; }

echo "measure-group started $(date -Is)"
echo "work dir: $WORK"

say "1. environment"
uname -a
if [ -r /proc/device-tree/model ]; then echo "model: $(tr -d '\0' </proc/device-tree/model)"; fi
echo "cpus: $(nproc)"
free -m 2>/dev/null | sed -n '1,2p'
vc() { command -v vcgencmd >/dev/null && "$@"; }
before_throttled=$(vc vcgencmd get_throttled 2>/dev/null || echo "n/a")
echo "throttled before: $before_throttled"
echo "arm clock before: $(vc vcgencmd measure_clock arm 2>/dev/null || echo n/a)"
echo "temperature before: $(vc vcgencmd measure_temp 2>/dev/null || echo n/a)"
if [ "$(id -u)" != 0 ]; then echo "note: not root (fine: no radio is opened)"; fi

say "2. binary"
BIN=${MEASURE_BIN:-}
if [ -z "$BIN" ]; then
    for c in "$(command -v decdrm 2>/dev/null)" \
             "$PWD/target/release/decdrm" \
             "$HOME/project/github/Ageres/DecDRM-fork/target/release/decdrm" \
             /home/*/project/github/Ageres/DecDRM-fork/target/release/decdrm \
             "$HOME/DecDRM-fork/target/release/decdrm" \
             /home/*/DecDRM-fork/target/release/decdrm; do
        if [ -n "$c" ] && [ -x "$c" ]; then BIN=$c; break; fi
    done
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
    say "SUMMARY"
    echo "ERROR: no decdrm binary; pass MEASURE_BIN=/path/to/target/release/decdrm"
    echo "log: $LOG"
    exit 1
fi
echo "binary: $BIN"
ls -l "$BIN"
sha256sum "$BIN" 2>/dev/null || true

# A group of <carriers> test tones <spacing> Hz apart, its composite to a WAV. The
# levels leave room for the sum's OFDM peaks (about 14 dB above the RMS).
write_group() { # <toml> <carriers> <spacing_hz> [resampler]
    local path=$1 n=$2 spacing=$3 resampler=${4:-fast}
    local level=-20
    [ "$n" -ge 4 ] && level=-21
    {
        printf '[output]\nformat = "iq"\nfile = "%s"\nsample_rate_hz = %s\n' "${path%.toml}.wav" "$RATE"
        printf 'resampler = "%s"\nlevel_dbfs = %s\n' "$resampler" "$level"
        local i=0
        while [ "$i" -lt "$n" ]; do
            local frequency=$((FIRST_HZ + i * spacing))
            printf '\n[[multiplex]]\nfrequency_hz = %s\nmode = "B"\noccupancy = 3\nlevel_dbfs = %s\n' \
                "$frequency" "$level"
            printf '\n[[multiplex.service]]\nlabel = "T%s"\nid = %s\n' "$i" "$(printf '0x%X' $((0x42 + i)))"
            printf '\n[multiplex.service.audio]\ncodec = "he-aac"\ninput = { tone_hz = %s.0 }\n' \
                $((440 + 220 * i))
            i=$((i + 1))
        done
    } >"$path"
}

run_case() { # <label> <carriers> <spacing_hz>
    local label=$1 n=$2 spacing=$3
    local name=${label// /_}
    local cfg="$WORK/$name.toml" out="$WORK/$name.out" wav="$WORK/$name.wav"
    write_group "$cfg" "$n" "$spacing"
    local strategy
    strategy=$("$BIN" tx "$cfg" --check 2>&1 | grep -E 'span:' | sed 's/^ *//' | head -1)
    say "$label: $n carrier(s), ${spacing} Hz apart, ${DUR}s of signal"
    echo "  $strategy"
    rm -f "$wav"
    local timed
    timed=$( { TIMEFORMAT='%R %U %S'; time "$BIN" tx "$cfg" --duration "$DUR" >"$out" 2>&1; } 2>&1 | tail -1 )
    grep -E '\] output |transmitted ' "$out" | tail -2 | sed 's/^/  /'
    local real user sys cpu rt
    real=$(echo "$timed" | awk '{print $1}')
    user=$(echo "$timed" | awk '{print $2}')
    sys=$(echo "$timed" | awk '{print $3}')
    if [ -z "$real" ] || [ "$real" = "0" ]; then
        echo "  FAILED: $timed (see $out)"
        printf '%s\t%s\tfailed\n' "$label" "$n" >>"$ROWS"
        rm -f "$wav"
        return
    fi
    cpu=$(awk -v r="$real" -v u="$user" -v s="$sys" 'BEGIN { if (r > 0) printf "%.0f", 100 * (u + s) / r; else printf "-" }')
    rt=$(awk -v d="$DUR" -v r="$real" 'BEGIN { if (r > 0) printf "%.2f", d / r; else printf "-" }')
    echo "  elapsed ${real} s (user ${user} s, sys ${sys} s), CPU ${cpu}% of one core, ${rt}x real time"
    printf '%s\t%s\t%s\t%s\t%s\n' "$label" "$n" "$cpu" "$rt" "$strategy" >>"$ROWS"
    rm -f "$wav"
}

say "3. cases"
run_case "1 carrier" 1 0
run_case "2 narrow" 2 20000
run_case "3 close" 3 70000
run_case "4 at the limit" 4 60000
run_case "2 wide" 2 300000
run_case "4 wide" 4 300000

say "4. environment after"
echo "throttled after: $(vc vcgencmd get_throttled 2>/dev/null || echo n/a)"
echo "arm clock after: $(vc vcgencmd measure_clock arm 2>/dev/null || echo n/a)"
echo "temperature after: $(vc vcgencmd measure_temp 2>/dev/null || echo n/a)"

say "SUMMARY (copy into docs/design/multi-multiplex.md: CPU of one core, x real time)"
echo "| Case | Carriers | CPU, % of one core | x real time | Strategy |"
echo "|---|---|---|---|---|"
while IFS=$'\t' read -r label n cpu rt strategy; do
    printf '| %s | %s | %s | %s | %s |\n' "$label" "$n" "$cpu" "$rt" "$strategy"
done <"$ROWS"
echo
echo "throttled: $before_throttled -> $(vc vcgencmd get_throttled 2>/dev/null || echo n/a)"
echo "log: $LOG"
