# Chrome writes to a .crdownload file and renames it when the download ends.
printf 'sample,ph\nS01-1,6.1\n' > 'Unconfirmed 381022.crdownload'
mv 'Unconfirmed 381022.crdownload' campaign-2026-11.csv
wait_for_verdict

# Firefox creates an empty file under the final name and replaces it with the .part file.
: > campaign-2026-12.csv
printf 'sample,ph\nS01-1,6.2\n' > campaign-2026-12.csv.part
mv campaign-2026-12.csv.part campaign-2026-12.csv
wait_for_verdict

# A .part file that stays longer than a debounce is left out until it is renamed.
printf 'sample,ph\nS01-1,6.3\n' > campaign-2027-01.csv.part
sleep 2
mv campaign-2027-01.csv.part campaign-2027-01.csv
wait_for_verdict

# Firefox's empty file under the final name is a file of its own when the download is slow, and
# the .part file renamed over it changes it.
: > campaign-2027-02.csv
printf 'sample,ph\nS01-1,6.4\n' > campaign-2027-02.csv.part
sleep 2
mv campaign-2027-02.csv.part campaign-2027-02.csv
wait_for_verdict 2
