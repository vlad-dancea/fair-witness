# A new file every twentieth of a second for 20 seconds, longer than a set may grow.
end=$((SECONDS + 20))
i=0
while [ "$SECONDS" -lt "$end" ]; do
  i=$((i + 1))
  printf 'sample,ph\nS%s,6.4\n' "$i" > "raw/sample-$i.csv"
  sleep 0.05
done
wait_for_verdict 2
