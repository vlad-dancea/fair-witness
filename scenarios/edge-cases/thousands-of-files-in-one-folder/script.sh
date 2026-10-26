mkdir sensors
for i in $(seq -w 1 3000); do printf 'sample,ph\nS%s,6.4\n' "$i" > "sensors/reading-$i.csv"; done
wait_for_verdict
