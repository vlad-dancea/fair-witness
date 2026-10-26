# Swaps two files through a temporary name.
mv site-a.csv swap.tmp
mv site-b.csv site-a.csv
mv swap.tmp site-b.csv
wait_for_verdict

mv site-c.csv site-d.csv
mv site-d.csv site-e.csv
wait_for_verdict
