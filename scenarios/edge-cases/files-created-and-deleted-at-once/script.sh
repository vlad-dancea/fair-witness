# A file that is gone before fair-witness looks, next to one that stays.
printf 'temp\n' > scratch.tmp
rm scratch.tmp
printf 'sample,ph\nS01-1,6.4\n' > site-01.csv
wait_for_verdict

# Only a file that comes and goes, which must not make a set of its own.
printf 'temp\n' > gone.tmp
rm gone.tmp
sleep 2
printf 'sample,ph\nS02-1,6.1\n' > site-02.csv
wait_for_verdict
