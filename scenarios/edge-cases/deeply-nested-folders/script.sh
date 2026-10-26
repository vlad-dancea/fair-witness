path=deep
for level in $(seq -w 1 40); do path="$path/level-$level"; done
mkdir -p "$path"
printf 'sample,ph\nS01-1,6.4\n' > "$path/site-01.csv"
wait_for_verdict
