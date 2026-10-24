mkdir -p analysis/__pycache__ analysis/.ipynb_checkpoints
printf 'import pandas as pd\n\npd.read_csv("raw/site-01.csv").plot(y="ph")\n' > analysis/plot_ph.py
printf 'import pandas as pd\n' > analysis/plot_ph.py~
printf 'compiled' > analysis/__pycache__/plot_ph.cpython-313.pyc
printf '{"cells": []}\n' > analysis/explore.ipynb
printf '{"cells": []}\n' > analysis/.ipynb_checkpoints/explore-checkpoint.ipynb
wait_for_verdict
