printf 'site,ph,moisture,nitrogen\nS01,6.5,0.21,0.14\n' > campaign-2026-05.csv
wait_for_verdict

printf 'Site S01 was flooded in May.\n' > notes.txt
wait_for_verdict
