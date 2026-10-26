# Stands in for the browser's Downloads folder, outside the project.
downloads=$(mktemp -d)

# GeoSphere Austria's data hub hands out five years of 10-minute readings of one station as a
# ZIP archive of monthly CSV files.
name=klima-v2-10min_5925_wien-hohe-warte_2021-2025
mkdir "$downloads/$name"
for year in 2021 2022 2023 2024 2025; do
  for month in 01 02 03 04 05 06 07 08 09 10 11 12; do
    awk -v year="$year" -v month="$month" 'BEGIN {
      srand(year * 12 + month)
      print "time,station,tl,rf,ff,cglo"
      days = 31
      if (month == 4 || month == 6 || month == 9 || month == 11) days = 30
      if (month == 2) days = year % 4 == 0 ? 29 : 28
      for (day = 1; day <= days; day++) {
        for (minute = 0; minute < 1440; minute += 10) {
          sun = sin((minute / 60 - 6) / 12 * 3.14159)
          if (sun < 0) sun = 0
          printf "%s-%s-%02dT%02d:%02d+00:00,5925,%.1f,%.0f,%.1f,%.1f\n", year, month, day,
            int(minute / 60), minute % 60, 12 + 10 * sun - (month - 7) ^ 2 / 3 + rand(),
            80 - 30 * sun + rand() * 5, rand() * 6, 600 * sun * rand()
        }
      }
    }' > "$downloads/$name/$name-$year-$month.csv"
  done
done
printf 'Data: GeoSphere Austria, https://data.hub.geosphere.at, CC BY 4.0\n' > "$downloads/$name/LICENSE.txt"
(cd "$downloads" && zip -qr "$name.zip" "$name")

# The researcher moves the download into the project, extracts it in the file manager a few
# seconds later and deletes the archive once they see the files are there.
mkdir reference
mv "$downloads/$name.zip" reference/
sleep 5
unzip -q "reference/$name.zip" -d reference
sleep 10
rm "reference/$name.zip"
wait_for_verdict 3
