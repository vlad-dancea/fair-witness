# The facility puts the reads of the run and their checksums up for download. Makes them outside
# the project, each starting like a gzip file and padded to its size without taking up the disk.
facility=$(mktemp -d)
number=0
for sample in $(for site in $(seq -w 1 39); do echo "S$site"; done) Undetermined; do
  number=$((number + 1))
  for read in R1 R2; do
    file="$facility/${sample}_S${number}_L001_${read}_001.fastq.gz"
    printf '\037\213\010\004\000\000\000\000\000\377\006\000BC\002\000' > "$file"
    truncate -s $((14000000 + (number * 7919 + ${read#R} * 104729) % 9000000)) "$file"
  done
done
(cd "$facility" && md5sum ./*.fastq.gz | sed 's| \./| |' > MD5SUMS.txt)

# The researcher makes a folder for the run and, once they have pasted the command from the
# facility's mail, wget downloads the files one after another.
mkdir -p reads/2026-10-run
sleep 6
for file in "$facility"/*.fastq.gz "$facility/MD5SUMS.txt"; do
  cp --sparse=always "$file" reads/2026-10-run/
  sleep 0.3
done
wait_for_verdict 2

# Later they check the files against MD5SUMS.txt, which changes nothing, and delete the reads
# the sequencer could not assign to a sample.
rm reads/2026-10-run/Undetermined_*
wait_for_verdict
