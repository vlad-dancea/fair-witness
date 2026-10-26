# The facility's mail lists the URLs of the reads. The researcher makes a folder for the run and,
# once they have pasted the command, wget downloads the files one after another. Each starts like
# a gzip file and is padded to its size without taking up the disk.
mkdir -p reads/2026-10-run
sleep 6
cd reads/2026-10-run
samples=$(for site in $(seq -w 1 39); do echo "S$site"; done; echo Undetermined)
number=0
for sample in $samples; do
  number=$((number + 1))
  for read in R1 R2; do
    file="${sample}_S${number}_L001_${read}_001.fastq.gz"
    printf '\037\213\010\004\000\000\000\000\000\377\006\000BC\002\000' > "$file"
    sleep 0.15
    truncate -s $((14000000 + (number * 7919 + ${read#R} * 104729) % 9000000)) "$file"
    sleep 0.15
  done
done
for file in *.fastq.gz; do
  echo "$(printf '%s' "$file" | md5sum | cut -c1-32)  $file"
done > MD5SUMS.txt
cd ../..
wait_for_verdict 2

# Later they check the files against MD5SUMS.txt, which changes nothing, and delete the reads
# the sequencer could not assign to a sample.
rm reads/2026-10-run/Undetermined_*
wait_for_verdict
