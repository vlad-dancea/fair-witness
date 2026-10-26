for i in $(seq 1 300); do printf 'sample,ph\nS01-1,6.%s\n' "$i" > data/campaign-2026-04.csv; done
for i in $(seq 1 300); do printf 'sample,ph\nS01-1,6.%s\n' "$i" > data/campaign-2026-10.csv; done
wait_for_verdict
