# fair-witness

Watches a project folder and flags changes that contradict its data management plan (DMP) in [DAMAP](https://github.com/tuwien-csd/damap-backend).

Each time files in the folder are created, changed, renamed or deleted, an AI model compares them with the DMP and tells you whether they are in line with it, for example when patient data lands in a project whose DMP says it holds no personal data.

## What you need

- **A DAMAP instance** you can log in to, with at least one DMP. The default is TU Wien's, [damap.it.tuwien.ac.at](https://damap.it.tuwien.ac.at). To run one locally, see [Running DAMAP locally](#running-damap-locally).
- **An AI model behind an OpenAI-compatible API.** The default is TU Wien's [Aqueduct](https://aqueduct.ai.datalab.tuwien.ac.at) gateway (`https://aqueduct.ai.datalab.tuwien.ac.at/v1`); create an API token in its web interface. A local [Ollama](https://ollama.com) (`http://localhost:11434/v1`), OpenAI, or anything else that serves `/v1/models` and `/v1/chat/completions` with tool calling works too.

## Install

macOS and Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/vlad-dancea/fair-witness/releases/latest/download/fair-witness-installer.sh | sh
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/vlad-dancea/fair-witness/releases/latest/download/fair-witness-installer.ps1 | iex"
```

Check that it works with `fair-witness --version`.

## Set up a folder

Go to the folder you want to watch and run `setup`:

```sh
cd ~/research/my-project
fair-witness setup
```

It asks four things, in order:

1. **DAMAP URL.** The address you open DAMAP at in the browser (default `https://damap.it.tuwien.ac.at`).
2. **Login.** A browser window opens on DAMAP's login page; confirm the code shown in the terminal. If DAMAP's login server does not allow browser login, you're asked for your username and password instead.
3. **DMP.** Pick the DMP that describes this folder. Type to filter the list.
4. **AI model.** Enter the API's URL (default Aqueduct) and, if it needs one, an API key. Aqueduct does. Then pick a model from the ones the API offers.

```
$ fair-witness setup
DAMAP URL (as opened in the browser) [https://damap.it.tuwien.ac.at]:
Connected to DAMAP Tool at https://damap.it.tuwien.ac.at
To log in, open https://damap.it.tuwien.ac.at/auth/realms/damap/device?user_code=ABCD-EFGH
and confirm the code ABCD-EFGH
DMP for this folder (type to filter): #3 Soil samples 2026, changed 2026-09-30
AI API URL (OpenAI-compatible) [https://aqueduct.ai.datalab.tuwien.ac.at/v1]:
API key (empty for none): ********
Model (type to filter): your-model
Changes will be checked with your-model.
Run `fair-witness run` to start watching this folder.
Edit /home/you/research/my-project/.fair-witness/ignore to choose which paths to ignore.
```

Run `setup` again any time to switch DMP, model or DAMAP instance. It remembers your previous answers and offers to keep you logged in.

### Setting up without prompts

The DAMAP URL and the AI settings can also be passed as flags or environment variables. That's handy for scripts or for keeping an API key out of your shell history:

| Flag | Environment variable | |
|---|---|---|
| `--url` | | DAMAP URL |
| `--ai-url` | `FAIR_WITNESS_AI_URL` | Base URL of the OpenAI-compatible API |
| `--api-key` | `FAIR_WITNESS_API_KEY` | API key for it |
| `--model` | `FAIR_WITNESS_MODEL` | Model to use |

```sh
FAIR_WITNESS_API_KEY=sk-... fair-witness setup --url https://damap.example.org \
  --ai-url https://api.openai.com/v1 --model gpt-5-mini
```

You still log in and pick the DMP interactively.

### What setup saves

Everything goes into a `.fair-witness/` folder at the root of the project:

- `config.toml`: DAMAP URL, DMP id, AI URL, model and effort. Safe to commit.
- `credentials.toml`: your DAMAP session and the API key. On macOS and Linux, readable only by you.
- `madmp.json`: a copy of the DMP chosen in `setup`, with the DAMAP it came from, refreshed on every `run`. When DAMAP cannot be reached, `run` checks against this copy and warns that it does.
- `ignore`: a commented template for paths to leave out of checks. `setup` creates it if it is missing and leaves an existing file unchanged.

A `.gitignore` inside it keeps `credentials.toml` and `madmp.json` out of git. You can run `fair-witness` from any subfolder. It finds `.fair-witness/` by looking upwards.

### What is ignored

Some files in a project are not research data, and checking them would only cost time and tokens. `run` does not report changes to:

- `.fair-witness/` itself
- `.git`, so a commit does not count as a change
- folders tools fill on their own: `.venv/`, `venv/`, `__pycache__/`, `node_modules/` and `.ipynb_checkpoints/`
- folders a tool tagged as a cache with a [`CACHEDIR.TAG`](https://bford.info/cachedir/) file, as cargo tags `target/` and pytest, mypy and ruff tag their `.pytest_cache/`, `.mypy_cache/` and `.ruff_cache/`
- editor swap and backup files (`*.swp`, `*.swo`, `*.swx`, `*~`, `.#*`, `#*#`) and `.DS_Store`

To ignore more, edit `.fair-witness/ignore`, which `setup` creates with a commented template. It uses the same syntax as a `.gitignore`. Paths are relative to the project root, and a line starting with `!` undoes a rule, including a built-in one or a folder's cache tag. `.fair-witness/` itself is always skipped:

```gitignore
# scratch output of the analysis scripts
scratch/
*.log
# this project keeps data in a folder called venv
!venv/
```

The file is read when `run` starts, so restart it after a change. The project's own `.gitignore` is not used, because data kept out of git is often exactly the data the DMP is about.

## Tutorial: watching a folder

### 1. Start watching

```sh
fair-witness run
```

The DMP is downloaded and the screen opens:

```
╭ fair-witness ─────────────────────────────────────────────────────╮
│Watching /home/you/research/my-project                             │
│Comparing changes with project #3 - Soil samples 2026.             │
│Using your-model from https://aqueduct.ai.datalab.tuwien.ac.at/v1. │
╰───────────────────────────────────────────────────────────────────╯
╭ Changes ──────────────────────────────────────────────────────────╮
│No changes yet.                                                    │
╰───────────────────────────────────────────────────────────────────╯
 q quit  ↑↓ PgUp PgDn scroll  End latest  d debug  m model
```

- The **header** shows the folder, the DMP and the model.
- The **Changes** pane lists what happens in the folder and what the AI makes of it.
- The **bottom line** lists the keys you can press.

Leave it running while you work. Use your file manager, scripts or any other tool as usual.

### 2. Make a change

In another terminal, or in your file manager, add a file to the project:

```sh
mkdir -p data
printf 'name,birthdate,diagnosis\nAnna Muster,1980-04-02,asthma\n' > data/patients.csv
```

About two seconds after the folder goes quiet, the changes show up as one numbered **change set**:

```
created: data                                                    #1
created: data/patients.csv                                       #1
  ↳ checking against the DMP… 4s
```

Changes that happen close together are grouped into one set. `created`, `changed`, `renamed` and `deleted` are colored so you can tell them apart. Files that are not research data are left out, as described in [What is ignored](#what-is-ignored).

A set of more than 50 changes is listed per folder instead, for you and for the AI, such as `created: 412 paths under data/raw/`. The AI can still list the folder to see what is in it.

### 3. Read the verdict

When the AI is done, its verdict appears under the set it belongs to:

```
created: data                                                    #1
created: data/patients.csv                                       #1
 ↳ contradicts the DMP (dataset[0].personal_data): The file holds
   names and diagnoses of patients, but the DMP says the project
   collects no personal data.
```

- **`in line with the DMP`** (green): nothing to do.
- **`contradicts the DMP`** (red): the AI names the DMP field it clashes with and explains why. You also get a **desktop notification**, so you see it even when the terminal is in the background.
- **`review failed`**: the model could not be reached, or gave no verdict. The reason is shown next to it.

To check its verdict, the AI only gets the DMP and the list of changed paths. When the names alone are not enough, it can list folders and read the first 16 KB of files. It cannot look outside the project folder or into `.fair-witness/`.

What you do about a contradiction is up to you. Either undo the change, or, if the plan has changed, update the DMP in DAMAP and restart `run` so it picks up the new version.

### 4. See what the AI did

Press `d` to open the **debug pane** next to the change list. It shows each review step by step: the prompt with the DMP, every request to the model, the tools it called with their results, its reasoning (if the model shares it) and the token counts.

- `Tab` switches which pane the arrow keys scroll.
- `c` collapses or expands long blocks, such as the DMP JSON.
- `d` hides the pane again.

This is the place to look when a verdict seems wrong.

### 5. Switch the model

Press `m` to change the model without restarting:

- Type to filter the model list, and use `↑↓` to choose.
- `Tab` switches to the **effort** column (`default`, `none`, `low`, `medium`, `high`) for models that support reasoning effort.
- `Enter` uses the choice from the next change set on and saves it to `config.toml`, unless `--ai-url` named another API than the one chosen in `setup`. `Esc` cancels.

To use a different API or key, quit and run `fair-witness setup`.

### 6. Stop

Press `q`, `Esc` or `Ctrl+C`. Your login is kept, so the next `run` starts right away.

### Keys

| Key | Does |
|---|---|
| `↑` `↓` / `k` `j` | Scroll one line |
| `PgUp` `PgDn` | Scroll ten lines |
| `End` / `G` | Jump to the latest |
| `d` | Show or hide the debug pane |
| `Tab` | Switch pane (debug pane open) |
| `c` | Collapse or expand long blocks (debug pane open) |
| `m` | Change model and effort |
| `q` / `Esc` / `Ctrl+C` | Quit |

## Commands

| Command | Does |
|---|---|
| `fair-witness setup` | Set up the current folder, or change its DAMAP login, DMP or model |
| `fair-witness run` | Watch the folder and check changes against the DMP |
| `fair-witness run --dmp <id>` | Check against another DMP this time only, without changing the saved choice |
| `fair-witness run --headless` | Print what happens as lines of JSON instead of drawing the screen, see [Running from a script](#running-from-a-script) |
| `fair-witness logout` | Forget the saved DAMAP login for this folder |

If `run` finds no DMP or no model, it still watches and lists changes, but does not check them. The header says so.

If DAMAP cannot be reached, `run` checks changes against the copy of the DMP it saved last time, in `.fair-witness/madmp.json`, and the header warns about it. This works only for the DMP chosen in `setup`, not one given with `--dmp`.

### Running from a script

Scripts and agents can run fair-witness without a terminal and without DAMAP. These options of `run` work with or without `--headless`:

| Flag | Environment variable | |
|---|---|---|
| `--headless` | | Print each event as one line of JSON on stdout instead of drawing the screen |
| `--project <dir>` | | Watch this folder instead of the current one |
| `--dmp-file <path>` | | Check against the maDMP JSON in this file instead of downloading the DMP from DAMAP |
| `--ai-url` | `FAIR_WITNESS_AI_URL` | Base URL of the OpenAI-compatible API, instead of the one chosen in `setup` |
| `--api-key` | `FAIR_WITNESS_API_KEY` | API key for it |
| `--model` | `FAIR_WITNESS_MODEL` | Model to use, instead of the one chosen in `setup` |
| | `FAIR_WITNESS_DEBOUNCE_MS` | How long the folder has to be quiet before its changes count as one set, in milliseconds (default 2000) |

With `--dmp-file` and the AI flags, the folder does not need to be set up, and nothing is written into it:

```sh
fair-witness run --headless --project ~/research/my-project --dmp-file dmp.json \
  --ai-url http://localhost:11434/v1 --model qwen3:8b
```

```
{"event":"changes","set":1,"changes":[{"created":"data/patients.csv"}]}
{"event":"reviewing","set":1}
{"event":"trace","set":1,"trace":{...}}
{"event":"verdict","set":1,"verdict":{"contradicts":true,"dmp_field":"dataset[0].personal_data","explanation":"..."}}
```

Each event has the number of the change set it belongs to. `trace` events are the steps the debug pane shows, and a set ends with a `verdict` or a `review_failed`. Messages for people, such as which DMP is used, go to stderr.

`--headless` stops when stdin closes, on `Ctrl+C` or on SIGTERM, also while it is still starting, and exits with 0. If stdout is not read, so that it cannot write out the events it has within 2 seconds, it exits with 1 instead. It never asks you to log in to DAMAP: when the saved login has expired, it falls back to the saved copy of the DMP, or fails if there is none. It exits with 2 when it cannot start, such as in a folder that is not set up, and with 1 when it fails while watching.

The API key goes only to the API chosen in `setup`. With another `--ai-url`, pass its key with `--api-key`. A model chosen with `m` for such an API is used until you quit but not saved.

## Troubleshooting

- **"this folder is not set up yet"**: run `fair-witness setup` in the folder, or in one of its parent folders.
- **"Is DAMAP running?"**: the saved DAMAP URL can't be reached. Start DAMAP, or run `setup` to change the URL.
- **"… does not look like an OpenAI-compatible API"**: the AI URL usually needs to end in `/v1`.
- **"answered 401 Unauthorized"**: the API key is missing or wrong. Aqueduct always needs one.
- **"offers no models"**: with Ollama, install a model first, for example `ollama pull qwen3:8b`; otherwise pass `--model`.
- **"review failed"**: open the debug pane (`d`) to see the last request and the error. Small models sometimes don't call tools properly; try a larger one with `m`.
- **No desktop notification**: the change list shows "could not show a notification" with the reason. Verdicts still appear in the terminal.

## Running DAMAP locally

With Docker installed, start DAMAP's development setup from a clone of [damap-backend](https://github.com/tuwien-csd/damap-backend):

```sh
git clone https://github.com/tuwien-csd/damap-backend
cd damap-backend
docker compose up -d --wait   # DAMAP at http://localhost:8085
docker compose down           # to stop it
```

Then run `fair-witness setup --url http://localhost:8085` and log in as `user` with password `user`.

## Development

[mise](https://mise.jdx.dev) pins the Rust toolchain and defines the tasks, and CI runs the same ones:

```sh
mise install            # the pinned Rust toolchain, with clippy and rustfmt
mise run dev -- setup   # run fair-witness from source
mise run format         # format the sources
mise run check          # everything CI checks: formatting, clippy, tests
```

Clippy warnings fail the check.

The code is split into two crates in `crates/`. `fair-witness-core` is a library that watches the folder, talks to DAMAP and reviews changes, without any terminal code. `fair-witness` is the binary with the commands, the setup questions and the full-screen interface.

## Releasing

A push to `main` publishes a release once `mise run check` passes on it, if `main` differs from the latest release in `crates/`, `Cargo.toml`, `Cargo.lock`, `dist-workspace.toml` or `.github/workflows/release.yml`. Changes to anything else, such as CI, this README or `mise.toml`, would build the same binaries and are not released. The `publish` job in `.github/workflows/ci.yml` tags the next version (the `version` in `crates/fair-witness/Cargo.toml`, or the next patch if that is already released) and starts `release.yml`, which builds the binaries and installers for Linux, macOS and Windows and attaches them to a GitHub release. To start a new minor or major line, bump `version` in `crates/fair-witness/Cargo.toml`.

The release workflow is generated by [dist](https://github.com/axodotdev/cargo-dist); after changing `dist-workspace.toml`, run `dist generate`.
