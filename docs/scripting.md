# Running from a script

Scripts and agents can run fair-witness without a terminal and without DAMAP. These options of `run` work with or without `--headless`:

| Flag | Environment variable | |
|---|---|---|
| `--headless` | | Print each event as one line of JSON on stdout instead of drawing the screen |
| `--project <dir>` | | Run in this folder instead of the current one. As there, the project is the nearest folder upwards that is set up, or this folder itself if none is |
| `--dmp-file <path>` | | Check against the maDMP JSON in this file instead of downloading the DMP from DAMAP |
| `--ai-url` | `FAIR_WITNESS_AI_URL` | Base URL of the OpenAI-compatible API, instead of the one chosen in `setup` |
| `--api-key` | `FAIR_WITNESS_API_KEY` | API key for the API at `--ai-url`. Needs `--ai-url` |
| `--model` | `FAIR_WITNESS_MODEL` | Model to use, instead of the one chosen in `setup` |
| `--effort` | `FAIR_WITNESS_EFFORT` | How much the model reasons, instead of the effort chosen with `m`: a name the model takes, such as `low` or `xhigh`, which goes to the API as given. `default` sends no effort, so the model uses its own, and `none` asks for no reasoning, which some models ignore |
| `--debounce-ms <ms>` | `FAIR_WITNESS_DEBOUNCE_MS` | How long the folder has to be quiet before its changes count as one set, in milliseconds (default 2000). A set ends 30 times as long after its first change at the latest |

With `--dmp-file` and the AI flags, the folder does not need to be set up, and `run` writes nothing into a folder that is not:

```sh
fair-witness run --headless --project ~/research/my-project --dmp-file dmp.json \
  --ai-url http://localhost:11434/v1 --model qwen3:8b
```

```
{"time":"2026-10-23T13:48:20.104Z","event":"changes","set":1,"changes":[{"created":"data/patients.csv"}]}
{"time":"2026-10-23T13:48:20.105Z","event":"reviewing","set":1}
{"time":"2026-10-23T13:48:20.105Z","event":"trace","set":1,"trace":{...}}
{"time":"2026-10-23T13:48:24.913Z","event":"verdict","set":1,"verdict":{"contradicts":true,"dmp_field":"dataset[0].personal_data","explanation":"..."}}
```

## Events

Each line is the same as in the events log of `.fair-witness/log/`: the time, the event and the number of the change set it belongs to, which `watch_stopped` and `log_stopped` do not have. `trace` events are the steps the debug pane shows, and a set ends with a `verdict` or a `review_failed`. Without a DMP or a model, `run` lists the changes but checks none, so no set gets either. A `log_stopped` event says that the log could not be written and holds no events from then on. The `changes` of the next sets can come before that, while the set is still reviewed, since the sets are reviewed one after another. Paths are relative to the project folder and written as the Changes pane and the AI see them, so that every name fits on one line and none is lost: `/` between folders on every system, a backslash in a name as `\\`, line breaks and other control characters as `\n`, `\r`, `\t` or `\u{1b}`, and a byte that is not UTF-8 as `\xe9`. JSON then doubles each backslash, as in `{"created":"latin1-\\xe9t\\xe9.csv"}`. Messages for people, such as which DMP is used, go to stderr. A folder that is set up also gets its log, and one that never ran `setup` gets none.

## Stopping and exit codes

`--headless` stops when stdin closes, on `Ctrl+C` or on SIGTERM, also while it is still starting, and exits with 0. If stdout is not read, so that it cannot write out the events it has within 2 seconds, it exits with 1 instead. While it runs, at most 1,000 events wait to be written. When stdout is not read fast enough to catch up with the events for 30 seconds, it stops and exits with 1 as well. It never asks you to log in to DAMAP: when the saved login has expired, it falls back to the saved copy of the DMP, or fails if there is none. It exits with 2 when it cannot start, such as in a folder that is not set up or with a model or effort the API refuses, and with 1 when it fails while watching. When the project folder or a folder above it is moved, deleted or replaced, or cannot be watched again after the system dropped events, it prints a `watch_stopped` event with the same error that stderr shows and stops. A `watch_errors` event says that changes may go unseen, as described under [Troubleshooting](troubleshooting.md), and watching goes on.

## API keys

The saved API key goes only to the API it was saved for in `setup`. With another `--ai-url`, pass its key with `--api-key`. A key from `--api-key` or `FAIR_WITNESS_API_KEY` needs `--ai-url` or `FAIR_WITNESS_AI_URL`, so that a key exported for one API never goes to the API a folder was set up with. A model chosen with `m` for such an API is used until you quit but not saved. As in `setup`, a key goes over plain `http://` only to this machine, and `run` exits with 2 before it sends anything when one would go elsewhere, as described in [Plain HTTP](configuration.md#plain-http).
