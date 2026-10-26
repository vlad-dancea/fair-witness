# One flight: 12 tiles that start like a GeoTIFF and are padded to a tile's size without
# taking up the disk, and the drone's flight log.
flight=drone/2026-09-24-roof-16
mkdir -p "$flight"
for tile in $(seq -w 1 12); do
  printf 'II*\000\010\000\000\000\022\000\000\001\003\000' > "$flight/tile-$tile.tif"
  truncate -s $((150000000 + 10#$tile * 7700000)) "$flight/tile-$tile.tif"
done
cat > "$flight/flight-log.csv" <<'CSV'
time,latitude,longitude,altitude_m,heading_deg,battery_pct,camera_temperature_c
2026-09-24T13:02:11+02:00,48.20431,16.30982,60.1,90,98,31.2
2026-09-24T13:02:41+02:00,48.20431,16.31104,60.0,90,96,31.4
2026-09-24T13:03:11+02:00,48.20402,16.31104,60.2,270,94,31.5
2026-09-24T13:03:41+02:00,48.20402,16.30982,59.9,270,92,31.7
2026-09-24T13:04:11+02:00,48.20373,16.30982,60.0,90,90,31.8
CSV
wait_for_verdict
