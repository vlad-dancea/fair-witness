# A logger at site 3 creates the campaign's file and writes a sample to it every tenth of a
# second for five seconds, holding it open, so the file is still being written long after it
# appeared.
exec 3> site-03.csv
echo "sample,ph,moisture,nitrogen" >&3
for i in $(seq 1 50); do echo "S03-$i,6.4,0.31,0.12" >&3; sleep 0.1; done
exec 3>&-
wait_for_verdict

# The same at site 4, opening the file again for each sample, as `>>` in a loop does.
echo "sample,ph,moisture,nitrogen" > site-04.csv
for i in $(seq 1 50); do echo "S04-$i,6.1,0.28,0.10" >> site-04.csv; sleep 0.1; done
wait_for_verdict
