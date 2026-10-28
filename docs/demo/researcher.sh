#!/usr/bin/env bash
# What the researcher does in the recording: copies a week of readings from two street sensors
# into the project, then exports the wellbeing survey with the participants' names and e-mail
# addresses, which the DMP says the project does not collect.
set -euo pipefail

reviews() {
  cat .fair-witness/log/*-events.jsonl 2>/dev/null | grep -cE '"event":"(verdict|review_failed)"' || true
}

# The events log appears when `run` starts.
until compgen -G '.fair-witness/log/*-events.jsonl' >/dev/null; do sleep 0.5; done
sleep 4

for station in ws01 ws02; do
  awk -v station="$station" 'BEGIN {
    print "timestamp,station,air_temperature_c,relative_humidity_pct,surface_temperature_c"
    for (d = 1; d <= 7; d++) for (h = 0; h < 24; h++) {
      t = 24 + 7 * sin((h - 9) / 24 * 6.2832) + d / 5
      printf "2026-07-%02dT%02d:00:00+02:00,%s,%.1f,%d,%.1f\n", d, h, station, t, 70 - 2 * (t - 20), t + (h > 9 && h < 19 ? 11 : 1)
    }
  }' > "sensors/$station-2026-07-week1.csv"
done

until (( $(reviews) >= 1 )); do sleep 0.5; done
sleep 3

cat > survey/wellbeing-2026-07.csv <<'CSV'
response_id,submitted,name,email,district,age,sleep_hours,heat_bother_1_to_5,cooled_place
1,2026-07-03T21:14:09+02:00,Anna Gruber,anna.gruber@example.org,Favoriten,67,5.5,4,park
2,2026-07-03T21:40:51+02:00,Josef Huber,j.huber@example.org,Ottakring,34,6,3,none
3,2026-07-03T22:02:37+02:00,Fatma Yilmaz,fatma.yilmaz@example.org,Favoriten,45,4.5,5,none
4,2026-07-04T07:18:22+02:00,Lukas Bauer,lukas.bauer@example.org,Floridsdorf,29,7,2,water
5,2026-07-04T08:05:13+02:00,Maria Novak,m.novak@example.org,Ottakring,72,5,5,cooled room
6,2026-07-04T19:47:40+02:00,Stefan Wagner,stefan.wagner@example.org,Floridsdorf,51,6.5,3,park
CSV
