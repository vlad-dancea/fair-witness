mkdir -p raw/2026-10
for site in $(seq -w 1 40); do
  printf 'sample,ph,moisture,nitrogen\nS%s-1,6.%s,0.31,0.12\nS%s-2,6.%s,0.29,0.14\n' \
    "$site" "$site" "$site" "$site" > "raw/2026-10/site-$site.csv"
done
wait_for_verdict
