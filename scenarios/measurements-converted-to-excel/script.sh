mv data raw
wait_for_verdict

# Stands in for an Excel file, which is a ZIP archive.
printf 'PK\003\004' > raw/campaign-2026-04.xlsx
rm raw/campaign-2026-04.csv
wait_for_verdict
