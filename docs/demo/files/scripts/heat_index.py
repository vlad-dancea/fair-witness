"""Daily maximum air and surface temperature per station."""

import csv
import sys
from collections import defaultdict

maxima = defaultdict(lambda: (float("-inf"), float("-inf")))
for path in sys.argv[1:]:
    with open(path, newline="") as file:
        for row in csv.DictReader(file):
            key = (row["station"], row["timestamp"][:10])
            air, surface = maxima[key]
            maxima[key] = (
                max(air, float(row["air_temperature_c"])),
                max(surface, float(row["surface_temperature_c"])),
            )

writer = csv.writer(sys.stdout)
writer.writerow(["station", "day", "max_air_temperature_c", "max_surface_temperature_c"])
for (station, day), (air, surface) in sorted(maxima.items()):
    writer.writerow([station, day, air, surface])
