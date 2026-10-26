# A slow unzip or copy fills a new folder with a file every twentieth of a second for eight
# seconds, more than a debounce but less than 30.
mkdir -p raw/2026-11
for site in $(seq -w 1 160); do
  printf 'sample,ph,moisture,nitrogen\nS%s-1,6.4,0.31,0.12\n' "$site" > "raw/2026-11/site-$site.csv"
  sleep 0.05
done
wait_for_verdict
