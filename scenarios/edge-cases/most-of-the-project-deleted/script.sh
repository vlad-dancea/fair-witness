for campaign in 01 02 03 04 05; do
  mkdir -p "raw/campaign-$campaign"
  for site in $(seq -w 1 60); do printf 'sample,ph\nS%s,6.4\n' "$site" > "raw/campaign-$campaign/site-$site.csv"; done
done
printf 'One folder per campaign.\n' > raw/README.md
wait_for_verdict

rm -r raw/campaign-01 raw/campaign-02 raw/campaign-03 raw/campaign-04
wait_for_verdict
