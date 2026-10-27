touch campaign-2026-04.csv
chmod 600 notes.md
mkdir -p .fair-witness
printf 'not research data\n' > .fair-witness/scratch.txt
# The events log of a `fair-witness run` in a subfolder that was set up on its own.
mkdir -p archive/.fair-witness/log
printf '{"event":"changes"}\n' > archive/.fair-witness/log/events.jsonl
printf 'sample,ph\nS01-1,6.2\n' > campaign-2026-10.csv
wait_for_verdict
