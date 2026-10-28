#!/usr/bin/env bash
# Records docs/demo/demo.gif: `fair-witness run` reviewing two changes in a demo project.
#
# The project starts with the files in files/, and is set up against TU Wien's DAMAP as `user`
# with password `user` and the DMP #2751 "Urban Heat and Wellbeing in Vienna", whose wellbeing
# survey holds no personal data. A model on Aqueduct reviews the changes researcher.sh makes.
# Pass the Aqueduct API key in FAIR_WITNESS_API_KEY, and expect a desktop notification for the
# contradiction:
#
#   FAIR_WITNESS_API_KEY=<key> docs/demo/record.sh
#
# A model's verdicts vary, so watch the result before committing it.
#
# Needs mise, which brings vhs and ttyd, and bubblewrap, ffmpeg and Chromium.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)

if [[ ${1-} == sandbox ]]; then
  # The shell that vhs records. The project shows up as /home/researcher/urban-heat, so the
  # picture holds no path of the machine it was recorded on.
  exec bwrap --dev-bind / / --tmpfs /home \
    --bind "$FW_DEMO_PROJECT" /home/researcher/urban-heat \
    --ro-bind "$FW_DEMO_BIN" /home/researcher/.local/bin/fair-witness \
    --ro-bind "$here/researcher.sh" /home/researcher/.local/bin/researcher \
    --setenv HOME /home/researcher \
    --setenv PATH "/home/researcher/.local/bin:$PATH" \
    --setenv PS1 '\W $ ' \
    --chdir /home/researcher/urban-heat \
    bash --norc --noprofile
fi

if [[ -z ${FAIR_WITNESS_API_KEY-} ]]; then
  echo "Set FAIR_WITNESS_API_KEY to an API key for Aqueduct." >&2
  exit 2
fi

root=$(cd "$here/../.." && pwd)
cargo build --locked --release --manifest-path "$root/Cargo.toml" -p fair-witness
demo=$(mktemp -d)
# The project holds the DAMAP login and the API key once it is set up.
trap 'rm -r "$demo"' EXIT
cp -r "$here/files" "$demo/urban-heat"
# Settings exported for other work would override the ones the recording sets up.
unset FAIR_WITNESS_AI_URL FAIR_WITNESS_MODEL FAIR_WITNESS_EFFORT FAIR_WITNESS_DEBOUNCE_MS
export FW_DEMO_PROJECT="$demo/urban-heat" FW_DEMO_BIN="$root/target/release/fair-witness"
cd "$root"
mise exec vhs@0.12.1 ttyd@1.7.7 -- vhs docs/demo/demo.tape
