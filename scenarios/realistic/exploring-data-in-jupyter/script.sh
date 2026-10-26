# A notebook with the given cells, as JupyterLab writes it. A cell given as "plot" holds a
# matplotlib figure as base64 PNG, as an executed plotting cell does. The cells reach awk
# through the environment, since `awk -v` would turn their \" into bare quotes.
notebook() {
  cells="$*" awk 'BEGIN {
    cells = ENVIRON["cells"]
    srand(7)
    b64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    n = split(cells, source, "|")
    print "{\n \"cells\": ["
    for (i = 1; i <= n; i++) {
      printf "  {\n   \"cell_type\": \"code\",\n   \"execution_count\": %d,\n   \"id\": \"c%04d\",\n   \"metadata\": {},\n   \"outputs\": [", i, i * 37
      if (source[i] == "plot") {
        printf "\n    {\n     \"data\": {\n      \"image/png\": \"iVBORw0KGgoAAAANSUhEUgAA"
        for (c = 0; c < 38000; c++) printf "%s", substr(b64, int(rand() * 64) + 1, 1)
        printf "\",\n      \"text/plain\": [\"<Figure size 640x480 with 1 Axes>\"]\n     },\n     \"metadata\": {},\n     \"output_type\": \"display_data\"\n    }\n   "
        source[i] = "measurements.plot.scatter(x=\\\"moisture\\\", y=\\\"ph\\\")"
      }
      printf "],\n   \"source\": [\"%s\"]\n  }%s\n", source[i], (i < n ? "," : "")
    }
    print " ],\n \"metadata\": {\n  \"kernelspec\": {\"display_name\": \"Python 3 (ipykernel)\", \"language\": \"python\", \"name\": \"python3\"},\n  \"language_info\": {\"name\": \"python\", \"version\": \"3.13.2\"}\n },\n \"nbformat\": 4,\n \"nbformat_minor\": 5\n}"
  }'
}

# Saves as Jupyter's atomic writing does: a backup copy next to the notebook, the notebook
# written in place, then the backup removed.
jupyter_save() {
  cp analysis/explore-ph.ipynb analysis/.~explore-ph.ipynb
  notebook "$@" > analysis/explore-ph.ipynb
  rm analysis/.~explore-ph.ipynb
}

# "New notebook" in JupyterLab's file browser creates Untitled.ipynb, which the researcher
# renames a few seconds later.
printf '{\n "cells": [],\n "metadata": {},\n "nbformat": 4,\n "nbformat_minor": 5\n}\n' > analysis/Untitled.ipynb
sleep 8
mv analysis/Untitled.ipynb analysis/explore-ph.ipynb

# JupyterLab autosaves every 2 minutes while there are unsaved changes. Pauses of 20 seconds
# here, since any pause longer than the debounce gives the same change sets.
sleep 20
jupyter_save 'import pandas as pd|measurements = pd.read_csv(\"../data/campaign-2026-04.csv\")|measurements.describe()'
sleep 20
jupyter_save 'import pandas as pd|measurements = pd.read_csv(\"../data/campaign-2026-04.csv\")|measurements.describe()|plot'
sleep 12
# Ctrl+S also writes a checkpoint, which fair-witness skips.
jupyter_save 'import pandas as pd|measurements = pd.read_csv(\"../data/campaign-2026-04.csv\")|measurements.describe()|plot|measurements.groupby(measurements[\"sample\"].str[:3])[\"ph\"].mean()'
mkdir -p analysis/.ipynb_checkpoints
cp analysis/explore-ph.ipynb analysis/.ipynb_checkpoints/explore-ph-checkpoint.ipynb
wait_for_verdict 5
