# A month of readings every 10 minutes, far more than read_file returns.
mkdir -p stations/roof-15-north
awk 'BEGIN {
  print "timestamp,air_temperature_c,relative_humidity_pct,wind_speed_ms,global_radiation_wm2"
  for (day = 1; day <= 30; day++) {
    for (minute = 0; minute < 1440; minute += 10) {
      hour = minute / 60
      sun = sin((hour - 6) / 12 * 3.14159)
      if (sun < 0) sun = 0
      printf "2026-09-%02dT%02d:%02d:00+02:00,%.1f,%.0f,%.1f,%.0f\n", day, int(hour), minute % 60,
        16 + 7 * sun - day / 10, 75 - 30 * sun, 1.5 + (minute % 70) / 30, 820 * sun
    }
  }
}' > stations/roof-15-north/station-2-2026-09.csv
wait_for_verdict
