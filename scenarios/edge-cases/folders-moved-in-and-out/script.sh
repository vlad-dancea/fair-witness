# Fills a folder outside the project, then moves it in whole.
outside=$(mktemp -d)
mkdir "$outside/campaign-2026-11"
for site in $(seq -w 1 20); do printf 'sample,ph\nS%s-1,6.2\n' "$site" > "$outside/campaign-2026-11/site-$site.csv"; done
mv "$outside/campaign-2026-11" campaign-2026-11
wait_for_verdict

mv campaign-2026-04 "$outside/"
wait_for_verdict
rm -r "$outside"
