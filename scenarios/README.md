# Scenarios

Each scenario is a scripted research session. A researcher changes files in a project folder while fair-witness watches it, and the scenario says what each review should find. `mise run scenarios` runs them against headless fair-witness with a fake model, and `mise run check` and CI run them too. There are two kinds, each in a folder of its own:

- `edge-cases/` try to break the watcher, the change sets and the model's tools, with changes picked for that rather than for being likely: thousands of files at once, odd file names, files moved in and out, renamed twice or created and deleted at once. They run at a short debounce and only with the fake model.
- `realistic/` are a researcher's everyday work at the pace they do it: downloading and extracting data, copying files from instruments, writing a script while pressing Ctrl+S, working in a notebook that autosaves. They run at fair-witness's own debounce, and with `--model` they run against real models, to measure how long reviews take, what they cost and how often a model gives the expected verdict. They are made up, so the verdicts give a rough idea of accuracy at best.

## Running them

```sh
mise run scenarios                    # every scenario
mise run scenarios -- clinic excel    # those whose names contain "clinic" or "excel"
mise run scenarios -- --help          # the other options
```

Names are unique across both folders. The edge cases take seconds each. A realistic scenario takes as long as the researcher would, up to a minute or two, since its pauses are real.

The fake model answers each review with the scenario's tool calls and then its expected verdict, so these runs check watching, grouping changes into sets, the tools and the events, but not how well a model judges. The runner prints which scenarios passed and failed, with what went wrong, and exits with 1 if any failed.

It writes `results.json` and the events of each run, one per line as `fair-witness run --headless` printed them, to a new folder in `target/scenarios/` for each run of the runner. The events are in `fake/<scenario>-<run>.jsonl`, and `results.json` names the file of each run. `--repeats` runs each scenario more than once, such as to catch a change set that splits only sometimes.

As many scenarios run at once as there are CPUs, but at most 8, since each holds an inotify instance and Linux allows a user 128 by default. `--jobs` changes the number.

### With a real model

```sh
read -rs FAIR_WITNESS_API_KEY && export FAIR_WITNESS_API_KEY   # paste the key, kept out of the history
mise run scenarios -- --model <model> --effort low --effort high --jobs 4
```

`--model` reviews the realistic scenarios with a model of an OpenAI-compatible API instead of the fake one, and skips the edge cases, which measure nothing. The API is TU Wien's Aqueduct unless `--ai-url` names another. Each `--model` runs at each `--effort`, by default only `default`. An effort is a name the model takes, such as `low` or `xhigh`, and goes to the API as given. `default` sends no effort, so the model uses its own, and `none` asks for no reasoning, which some models ignore. A model's verdicts vary, so each scenario runs 5 times with each model and effort, and `--repeats` changes that. Such a run costs tokens and takes minutes, so it happens only when asked, never in `mise run check` or CI.

Before the runs, the runner asks the API once whether it takes each model and effort, with a request like the first of a review but cut off after one token, as `fair-witness run` does when it starts. A model and effort the API refuses does not run. The runner lists it under "Refused by the API" with the API's message, keeps it in `refused` in `results.json` and exits with 1. When a check cannot tell, such as when the API answers `429 Too Many Requests`, the runner says so on stderr and runs the model anyway. Every run gets the hidden `--skip-model-check` of `fair-witness run`, so that each model and effort is checked once and not in every run, and the fake model needs no check.

The API key comes from `FAIR_WITNESS_API_KEY`, as for fair-witness itself. With a key, `--ai-url` takes plain `http://` only for this machine, such as for a stub API, so that the key never crosses the network unencrypted. The runner sends it to the API for its checks and hands it to fair-witness in its environment, never on a command line or to the script. It replaces the key with `[API key]` in what the checks answer and in everything it keeps from fair-witness, such as an error the API sends back, however JSON escapes it. When escaped escapes leave unclear where the key is, the whole message is replaced. The runner takes only a key of printable ASCII characters without a backslash, which it can find behind every escape JSON has for them.

An API limits how many requests each model gets a minute, and a review usually takes 2 or 3. When the API answers `429 Too Many Requests`, fair-witness waits as long as it asks and sends the request again, up to 10 times. The seconds a review took then include its waits, and its events hold a `rate_limited` trace for each. The runs take turns between the models, but as many run at once as with the fake model. On Aqueduct, where some models allow 12.5 requests a minute, pass `--jobs 4`. With it, 30 reviews of `glm-5.3-744b-experimental` took under 4 minutes, and the 12 requests Aqueduct refused all went through on their first retry.

While the runs go on, the runner says on stderr how each one ended. Then it prints how often each model and effort gave the expected verdict, with the tokens and seconds a review took on average, how often each change set got it right, and why reviews failed, such as a wrong key or too many requests. The events of each run are in `<model>-<effort>/<scenario>-<run>.jsonl`, and `results.json` also holds the `accuracy` of each model and effort.

A verdict is right when it says whether the changes contradict the DMP as the scenario expects and, for a contradiction, names a field that `dmp_field` accepts. `wrong` counts the verdicts with the opposite answer, `wrong field` those that find the expected contradiction but name a field that `dmp_field` does not accept, such as two fields in one, and `failed` the sets without a verdict, because the review failed or never ended. A wrong verdict or a failed review is what the run measures, so it fails nothing. The runner lists the runs that went wrong in other ways, such as a set with other changes than expected, and exits with 1 if there were any.

## A scenario

```
realistic/clinic-export-with-patient-names/
  scenario.toml   what each change set should hold and the verdict it should get
  dmp.json        the maDMP the changes are checked against
  script.sh       what the researcher does
  files/          the files the project folder starts with, if any
```

The runner copies `files/` into a new folder in the system's temp folder, starts `fair-witness run --headless` on it with `dmp.json`, and runs `script.sh` there with bash.

### script.sh

The script changes files in the project folder, which is its working directory, and calls `wait_for_verdict` to wait for verdicts. `wait_for_verdict` waits for the verdict of the next change set, and `wait_for_verdict 3` for the next three. The script waits once for each set in `scenario.toml` in all and changes nothing after the last wait. A script can go on changing files while the sets before are reviewed and wait for their verdicts later, as a researcher who keeps working does.

fair-witness ends a change set once the folder has been quiet for the debounce, or once the set's first change is 30 debounces old, as in a long copy that never leaves the folder quiet. The debounce is 500 milliseconds for the edge cases (`--debounce-ms` changes it) and fair-witness's own 2 seconds for the realistic scenarios. Changes less than a debounce apart end up in the same set, and a `sleep` or a slow command that leaves the folder quiet for longer than a debounce and a quarter splits the set in two. A pause between the two can go either way, because fair-witness looks at the files of a set that is still open only every quarter debounce, so keep a pause that should split a set well over it. fair-witness sees a change only once the change is a debounce old, so a set comes about two and a half debounces after its last change. Writes to a file that stays open count only once they pause for a debounce.

A script runs for at most 5 minutes before its first wait, between two waits and after its last.

### Writing a realistic scenario

- Do what the tools do, not just what the researcher means: a browser download lands in Downloads and is moved in, an editor that saves in place truncates and rewrites the file, Jupyter copies the notebook to `.~name` before it writes it, an office suite leaves a lock file while a document is open, a file manager extracts an archive in one burst.
- Keep the pace of a person within a task. Ctrl+S pressed twice in a row is under a second apart and makes one set, while saves between bursts of typing are seconds apart and make a set each. Write those pauses as `sleep`, so that with a real model the next set can arrive while the review before is still running.
- Between tasks that a researcher would do hours or days apart, such as two daily downloads, call `wait_for_verdict` instead of sleeping. Nothing else happens in the folder in between, and each review stands on its own.
- Shorten a pause only where the length cannot change the sets, such as an autosave every 2 minutes, which makes the same sets every 20 seconds.
- Remove what the script puts outside the project when it ends, as with `trap 'rm -r "$downloads"' EXIT`, since every check runs the scenarios and the system's temp folder may be small.
- Give files the shape and size of the real thing, such as the columns and header lines an instrument or a survey tool exports. Pad large files with `truncate`, which takes no disk space, after their first bytes, and write a large copy in steps, as a copy runs at the speed of a card or a network.

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
| `changes` | The changes fair-witness should report, in any order, as its `changes` events give them: `created`, `changed` or `deleted` with a path, or `renamed` with `from` and `to`. A file created in the set and written again under the same name is only `created`, and a file changed more than once is `changed` once. Paths are relative to the project folder and escaped as the events write them, such as `'latin1-\xe9t\xe9.csv'` for a name that is not UTF-8, in single quotes so that TOML keeps the backslashes. Leave it out to skip the comparison, such as for a bulk import |
| `contradicts` | Whether the changes contradict the DMP. Required |
| `dmp_field` | The DMP field they contradict, such as `dataset[0].personal_data`, or a list of the fields a careful reviewer could name, only when `contradicts` is true. A verdict's field counts when it is one of them or a field inside one, such as `dataset[0].distribution[0].format[0]` for `dataset[0].distribution[0].format`, with or without a leading `dmp.`. Left out, any field counts. The fake model hands in the first |
| `explanation` | Why this verdict is right, for the person who checks the scenario. The fake model hands it in as its explanation, and a model's own explanation is not compared with it. Required |
| `fake_tool_calls` | The tools the fake model calls before its verdict, each with `tool` and `arguments`, such as `{ tool = "read_file", arguments = { path = "data/patients.csv" } }`. A real model chooses its own |

With the fake model, a set passes when its changes match, if given, and its review ends in a verdict that `contradicts` and `dmp_field` accept. A scenario passes when every set passes and nothing else went wrong, such as a failing script, a failing fake tool call, a change set the scenario does not expect, or fair-witness stopping with an error. With a real model, the verdicts and tool calls are the model's own, so only the rest counts.

### For the fake and the real models alike

A realistic scenario runs unchanged with both. The fake model reads its answer from `scenario.toml`, but a real model sees only the DMP, the list of changes and what it reads with `list_dir` and `read_file`, never the `description` or the `explanation`. So the expected verdict must follow from those alone, and the files must hold what a careful reviewer would read to decide, such as the names in a clinic export. When a careful reviewer could cite more than one field of the DMP, list each of them in `dmp_field`, so that a model is not counted wrong for a reasonable choice.
