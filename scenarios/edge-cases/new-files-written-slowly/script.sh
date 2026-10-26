# A logger creates a file and writes a line to it every tenth of a second for five seconds,
# holding it open, so the file is still being written long after it appeared.
exec 3> logger.csv
echo "time,ph" >&3
for i in $(seq 10 59); do echo "2026-10-26T10:00:$i,6.4" >&3; sleep 0.1; done
exec 3>&-
wait_for_verdict

# The same, opening the file again for each line, as `>>` in a loop does.
for i in $(seq 10 59); do echo "2026-10-26T10:01:$i,6.5" >> appended.csv; sleep 0.1; done
wait_for_verdict
