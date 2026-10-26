# Stands in for the browser's Downloads folder, outside the project.
downloads=$(mktemp -d)

# Writes the portal's CSV export of one day for all 12 stations.
export_day() {
  awk -v day="$1" 'BEGIN {
    srand(day)
    split("roof-10-north roof-10-centre roof-10-south roof-15-north roof-15-centre roof-15-south roof-16-north roof-16-centre roof-16-south roof-20-north roof-20-centre roof-20-south", stations, " ")
    printf "# GreenLogger Cloud export, 2026-10-%02d 00:00 to 23:50, Europe/Vienna\n", day
    print "station_id,timestamp,air_temperature_c,relative_humidity_pct,wind_speed_ms,global_radiation_wm2"
    for (s = 1; s <= 12; s++) {
      for (minute = 0; minute < 1440; minute += 10) {
        hour = minute / 60
        sun = sin((hour - 7) / 10 * 3.14159)
        if (sun < 0) sun = 0
        printf "%s,2026-10-%02dT%02d:%02d:00+02:00,%.1f,%.0f,%.1f,%.0f\n", stations[s], day,
          int(hour), minute % 60, 8 + 9 * sun + rand() * 1.5 - s / 12, 88 - 35 * sun + rand() * 6,
          rand() * 4 + s % 3, 540 * sun * (0.7 + rand() * 0.3)
      }
    }
  }'
}

# Each morning the researcher downloads the day before from the station portal and moves it from
# Downloads into the project.
mkdir -p stations/raw
export_day 20 > "$downloads/export_2026-10-20.csv"
mv "$downloads/export_2026-10-20.csv" stations/raw/2026-10-20.csv
wait_for_verdict

export_day 21 > "$downloads/export_2026-10-21.csv"
mv "$downloads/export_2026-10-21.csv" stations/raw/2026-10-21.csv
wait_for_verdict

# On the third morning they click the portal's Excel button by mistake. An Excel file is a ZIP
# archive, which starts like this, and is padded to the size of a day of readings.
printf 'PK\003\004\024\000\006\000\010\000\000\000!\000' > "$downloads/export_2026-10-22.xlsx"
truncate -s 61842 "$downloads/export_2026-10-22.xlsx"
mv "$downloads/export_2026-10-22.xlsx" stations/raw/2026-10-22.xlsx
wait_for_verdict
