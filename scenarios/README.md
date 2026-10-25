# Scenarios

Each folder here is a scripted research session. A researcher changes files in a project folder while fair-witness watches it, and the scenario says what each review should find. `mise run scenarios` runs them against headless fair-witness with a fake model, and `mise run check` and CI run them too. With `--model`, they run against real models instead, to measure how often each gives the expected verdict.

## Running them

```sh
mise run scenarios                    # every scenario
mise run scenarios -- clinic excel    # those whose names contain "clinic" or "excel"
mise run scenarios -- --help          # the other options
```

The fake model answers each review with the scenario's tool calls and then its expected verdict, so these runs check watching, grouping changes into sets, the tools and the events, but not how well a model judges. The runner prints which scenarios passed and failed, with what went wrong, and exits with 1 if any failed.

It writes `results.json` and the events of each run, one per line as `fair-witness run --headless` printed them, to a new folder in `target/scenarios/` for each run of the runner. The events are in `fake/<scenario>-<run>.jsonl`, and `results.json` names the file of each run. `--repeats` runs each scenario more than once, such as to catch a change set that splits only sometimes.

As many scenarios run at once as there are CPUs, but at most 8, since each holds an inotify instance and Linux allows a user 128 by default. `--jobs` changes the number.

### With a real model

```sh
read -rs FAIR_WITNESS_API_KEY && export FAIR_WITNESS_API_KEY   # paste the key, kept out of the history
mise run scenarios -- --model <model> --effort low --effort high --jobs 1
```

`--model` reviews with a model of an OpenAI-compatible API instead of the fake one, by default TU Wien's Aqueduct (`--ai-url` changes it). Each `--model` runs at each `--effort`: `default`, which sends no effort, `none`, `low`, `medium` or `high`, by default only `default`. A model's verdicts vary, so each scenario runs 5 times with each model and effort, and `--repeats` changes that. Such a run costs tokens and takes minutes, so it happens only when asked, never in `mise run check` or CI.

The API key comes from `FAIR_WITNESS_API_KEY`, as for fair-witness itself. With a key, `--ai-url` takes plain `http://` only for this machine, such as for a stub API, so that the key never crosses the network unencrypted. The runner hands it to fair-witness in its environment, never on a command line or to the script, and replaces it with `[API key]` in everything it keeps from fair-witness, such as an error the API sends back, however JSON escapes it. When escaped escapes leave unclear where the key is, the whole message is replaced. The runner takes only a key of printable ASCII characters without a backslash, which it can find behind every escape JSON has for them.

An API limits how many requests each model gets a minute, and a review that goes over fails with `429 Too Many Requests`. A review usually takes 2 or 3 requests. The runs take turns between the models, but as many run at once as with the fake model, which is too many for Aqueduct, where some models allow 12.5 requests a minute. Pass `--jobs 1` there, which runs 30 reviews per model and effort in about 6 minutes.

While the runs go on, the runner says on stderr how each one ended. Then it prints how often each model and effort gave the expected verdict, with the tokens and seconds a review took on average, how often each change set got it right, and why reviews failed, such as a wrong key or too many requests. The events of each run are in `<model>-<effort>/<scenario>-<run>.jsonl`, and `results.json` also holds the `accuracy` of each model and effort.

A verdict is right when it says whether the changes contradict the DMP as the scenario expects and, for a contradiction, names a field that `dmp_field` accepts. `wrong` counts the verdicts with the opposite answer, `wrong field` those that find the expected contradiction but name a field that `dmp_field` does not accept, such as two fields in one, and `failed` the sets without a verdict, because the review failed or never ended. A wrong verdict or a failed review is what the run measures, so it fails nothing. The runner lists the runs that went wrong in other ways, such as a set with other changes than expected, and exits with 1 if there were any.

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
| `dmp_field` | The DMP field they contradict, such as `dataset[0].personal_data`, or a list of the fields a careful reviewer could name, only when `contradicts` is true. A verdict's field counts when it is one of them or a field inside one, such as `dataset[0].distribution[0].format[0]` for `dataset[0].distribution[0].format`, with or without a leading `dmp.`. Left out, any field counts. The fake model hands in the first |
| `explanation` | Why this verdict is right, for the person who checks the scenario. The fake model hands it in as its explanation, and a model's own explanation is not compared with it. Required |
| `fake_tool_calls` | The tools the fake model calls before its verdict, each with `tool` and `arguments`, such as `{ tool = "read_file", arguments = { path = "data/patients.csv" } }`. A real model chooses its own |

With the fake model, a set passes when its changes match, if given, and its review ends in a verdict that `contradicts` and `dmp_field` accept. A scenario passes when every set passes and nothing else went wrong, such as a failing script, a failing fake tool call, a change set the scenario does not expect, or fair-witness stopping with an error. With a real model, the verdicts and tool calls are the model's own, so only the rest counts.

### For the fake and the real models alike

A scenario runs unchanged with both. The fake model reads its answer from `scenario.toml`, but a real model sees only the DMP, the list of changes and what it reads with `list_dir` and `read_file`, never the `description` or the `explanation`. So the expected verdict must follow from those alone, and the files must hold what a careful reviewer would read to decide, such as the names in a clinic export. When a careful reviewer could cite more than one field of the DMP, list each of them in `dmp_field`, so that a model is not counted wrong for a reasonable choice.
