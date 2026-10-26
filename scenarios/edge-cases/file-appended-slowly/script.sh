# Appends a line every tenth of a second for five seconds, opening the file for each line.
for i in $(seq -w 1 50); do echo "2026-10-26T10:00:$i,6.4" >> logger.csv; sleep 0.1; done
wait_for_verdict

# The same, with the file held open the whole time, as a logger does.
exec 3>> logger.csv
for i in $(seq -w 1 50); do echo "2026-10-26T11:00:$i,6.5" >&3; sleep 0.1; done
exec 3>&-
wait_for_verdict
