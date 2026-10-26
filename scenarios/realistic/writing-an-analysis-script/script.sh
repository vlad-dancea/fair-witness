# The researcher writes a plotting script in their editor, which saves it in place, and presses
# Ctrl+S whenever they pause, sometimes twice in a row out of habit.
mkdir analysis
save() {
  cat > analysis/plot_ph.py
}

save <<'PY'
import pandas as pd
PY
sleep 0.5
save <<'PY'
import pandas as pd
PY
sleep 14
save <<'PY'
import matplotlib.pyplot as plt
import pandas as pd

measurements = pd.read_csv("data/campaign-2026-04.csv")
PY
sleep 9
save <<'PY'
import matplotlib.pyplot as plt
import pandas as pd

measurements = pd.read_csv("data/campaign-2026-04.csv")
measurements["site"] = measurements["sample"].str.split("-").str[0]
by_site = measurements.groupby("site")["ph"].mean()
PY
sleep 0.7
save <<'PY'
import matplotlib.pyplot as plt
import pandas as pd

measurements = pd.read_csv("data/campaign-2026-04.csv")
measurements["site"] = measurements["sample"].str.split("-").str[0]
by_site = measurements.groupby("site")["ph"].mean()
PY
sleep 18
save <<'PY'
import matplotlib.pyplot as plt
import pandas as pd

measurements = pd.read_csv("data/campaign-2026-04.csv")
measurements["site"] = measurements["sample"].str.split("-").str[0]
by_site = measurements.groupby("site")["ph"].mean()

by_site.plot.bar(ylabel="pH")
plt.tight_layout()
plt.savefig("figures/ph-by-site.png", dpi=200)
PY
sleep 6
# They run it, and it writes the figure, which starts like a PNG file.
mkdir figures
printf '\211PNG\r\n\032\n\000\000\000\rIHDR\000\000\004\260\000\000\003\040' > figures/ph-by-site.png
truncate -s 48213 figures/ph-by-site.png
wait_for_verdict 5

# The axis should say which campaign it is, so they fix the label a few seconds later and run it
# again.
sleep 8
save <<'PY'
import matplotlib.pyplot as plt
import pandas as pd

measurements = pd.read_csv("data/campaign-2026-04.csv")
measurements["site"] = measurements["sample"].str.split("-").str[0]
by_site = measurements.groupby("site")["ph"].mean()

by_site.plot.bar(ylabel="pH", title="April 2026 campaign")
plt.tight_layout()
plt.savefig("figures/ph-by-site.png", dpi=200)
PY
sleep 4
printf '\211PNG\r\n\032\n\000\000\000\rIHDR\000\000\004\260\000\000\003\040' > figures/ph-by-site.png
truncate -s 51877 figures/ph-by-site.png
wait_for_verdict 2
