# Scenarios

Each folder here is a scripted research session. A researcher changes files in a project folder while fair-witness watches it, and the scenario says what each review should find. `mise run scenarios` runs them against headless fair-witness, and `mise run check` and CI run them too.

## Running them

```sh
mise run scenarios                    # every scenario
mise run scenarios -- clinic excel    # those whose names contain "clinic" or "excel"
mise run scenarios -- --help          # the other options
```

The runner prints which scenarios passed and failed, with what went wrong, and exits with 1 if any failed. It writes `results.json` and the events of each scenario, one per line as `fair-witness run --headless` printed them, to a new folder in `target/scenarios/` for each run.

As many scenarios run at once as there are CPUs, but at most 8, since each holds an inotify instance and Linux allows a user 128 by default. `--jobs` changes the number.

For now the runner only uses the fake model. It answers each review with the scenario's tool calls and then its expected verdict, so the scenarios check watching, grouping changes into sets, the tools and the events, but not how well a model judges.

## A scenario

```
clinic-export-with-patient-names/
  scenario.toml   what each change set should hold and the verdict it should get
  dmp.json        the maDMP the changes are checked against
  script.sh       what the researcher does
  files/          the files the project folder starts with, if any
```

The runner copies `files/` into a new folder in the system's temp folder, starts `fair-witness run --headless` on it with `dmp.json`, and runs `script.sh` there with bash.

### script.sh

The script changes files in the project folder, which is its working directory, and calls `wait_for_verdict` after each change set. `wait_for_verdict` returns once fair-witness has reviewed what changed since the last call. The script calls it once for each set in `scenario.toml` and changes nothing after the last call.

fair-witness takes each change into a set once the change is older than the debounce, 500 milliseconds for scenarios (`--debounce-ms` changes it), and checks for such changes four times per debounce. Changes made one right after the other end up in the same set, but a `sleep` or a slow command between them can split the set in two, which fails the scenario.

The script runs with `set -euo pipefail`, so a failing command fails the scenario. It must not read stdin, which `wait_for_verdict` uses. When the script ends, whatever it started in the background is stopped, unless it left the script's process group, as `setsid` does. Like a test, the script runs with the permissions of whoever runs the scenarios, so read a scenario before you run it.

```bash
mv data raw
wait_for_verdict

# Stands in for an Excel file, which is a ZIP archive.
printf 'PK\003\004' > raw/campaign-2026-04.xlsx
rm raw/campaign-2026-04.csv
wait_for_verdict
```

### scenario.toml

```toml
description = "A researcher renames the data folder, then converts a CSV file of measurements to Excel and deletes the CSV."

[[set]]
changes = [{ renamed = { from = "data", to = "raw" } }]
contradicts = false
explanation = "The DMP does not say where in the project the measurements are kept."

[[set]]
changes = [
  { created = "raw/campaign-2026-04.xlsx" },
  { deleted = "raw/campaign-2026-04.csv" },
]
contradicts = true
dmp_field = "dataset[0].distribution[0].format"
explanation = "The DMP keeps the soil measurements as CSV, and now they are an Excel file."
```

| Key | Meaning |
|---|---|
| `description` | What the researcher does, in a sentence or two. Required |
| `[[set]]` | One for each change set, in the order the script makes them |
| `changes` | The changes fair-witness should report, in any order, as its `changes` events give them: `created`, `changed` or `deleted` with a path, or `renamed` with `from` and `to`. Paths are relative to the project folder. Leave it out to skip the comparison, such as for a bulk import |
| `contradicts` | Whether the changes contradict the DMP. Required |
| `dmp_field` | The DMP field they contradict, such as `dataset[0].personal_data`, only when `contradicts` is true. Left out, any field counts |
| `explanation` | Why this verdict is right, for the person who checks the scenario. The fake model hands it in as its explanation, and a model's own explanation is not compared with it. Required |
| `fake_tool_calls` | The tools the fake model calls before its verdict, each with `tool` and `arguments`, such as `{ tool = "read_file", arguments = { path = "data/patients.csv" } }` |

A set passes when its changes match, if given, and its review ends in a verdict that matches `contradicts` and, for a contradiction, `dmp_field`. A scenario passes when every set passes and nothing else went wrong, such as a failing script, a change set the scenario does not expect, or fair-witness stopping with an error.
