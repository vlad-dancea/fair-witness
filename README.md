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

Run `setup` again any time to switch DMP, model or DAMAP instance. It remembers your previous answers and offers to keep you logged in. The saved API key goes only to the API it was saved for. For that API, an empty answer keeps the key, and so does `--ai-url` with the same URL and no `--api-key`. Another API gets only a key you give for it, and the saved one is replaced, or removed when you give none. To remove the saved key and keep the API, run `fair-witness setup --api-key ''`. It removes the key before it contacts the API, so the key is gone even when an API that needs one then refuses to answer. The effort chosen with `m` stays only when you keep the same API and model, because an effort one model takes can fail every review on another. When `setup` drops it, the line that names the model says so, and the model uses its default effort until you choose one with `m` in `run`.

Last, `setup` asks the API once whether it takes the model and the effort it keeps, as `run` does when it starts. If the API refuses the effort, `setup` shows the API's message, drops the effort and checks the model at its default effort. If the API refuses the model, `setup` shows the message and asks for another model, or stops when `--model` named it.

fair-witness talks to DAMAP over plain `http://` only on this machine: `localhost`, `127.0.0.0/8` and `[::1]`, such as a DAMAP [running locally](#running-damap-locally) at `http://localhost:8085`. Anywhere else the login would cross the network unencrypted, and anyone along the way could read it and act as you in DAMAP. With the username and password login, that includes your password. `setup` and `run` refuse such a DAMAP URL before they send anything to it, and the same for DAMAP's login server, whose address comes from DAMAP. They reach a DAMAP or login server on this machine directly, also when `HTTP_PROXY` or `ALL_PROXY` names a proxy.

An API key goes over plain `http://` only to this machine: `localhost`, `127.0.0.0/8` and `[::1]`. Anywhere else it would cross the network unencrypted, where anyone along the way could read it and use it. fair-witness reaches an API on this machine directly, also when `HTTP_PROXY` or `ALL_PROXY` names a proxy, which would get the key the same way. For an API on another machine, use `https://`, or reach it at `http://localhost` through an SSH tunnel, such as `ssh -L 11434:localhost:11434 lab-machine` for Ollama on a lab machine. For a plain `http://` address on another machine, `setup` neither asks for a key nor sends the saved one, so an API that needs no key still works, and it refuses a key given with `--api-key`.

### Setting up without prompts

The DAMAP URL and the AI settings can also be passed as flags or environment variables. That's handy for scripts or for keeping an API key out of your shell history:

| Flag | Environment variable | |
|---|---|---|
| `--url` | | DAMAP URL |
| `--ai-url` | `FAIR_WITNESS_AI_URL` | Base URL of the OpenAI-compatible API |
| `--api-key` | `FAIR_WITNESS_API_KEY` | API key for it, or empty to remove the saved one |
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
- `madmp.json`: a copy of the DMP chosen in `setup`, with the DAMAP it came from, refreshed on every `run`. When DAMAP cannot be reached, `run` checks against this copy and warns that it does. It names the DMP's contributors with their e-mail addresses, so on macOS and Linux it is readable only by you.
- `ignore`: a commented template for paths to leave out of checks. `setup` creates it if it is missing and leaves an existing file unchanged.
- `<file>.<random>.part`: what was left when fair-witness stopped while saving `<file>`. `config.toml`, `credentials.toml`, `madmp.json` and the `.gitignore` are written under such a name first and then renamed over the old file, so a save that fails keeps the old file whole. You can delete these.
- `log/`: what each `run` did, written by `run`. `<start time>-events.jsonl` has one line of JSON for every change set, every step of its review and the verdict, with the time, the model and effort, the token counts and how long each reply took. To keep it short, the system prompt, the DMP and the model are written once, before the first review, and again only when they change. `<start time>-diagnostics.log` has what happened outside the reviews, such as requests to DAMAP, folders that could not be read and notifications that failed. The `FAIR_WITNESS_LOG` environment variable sets how much it holds: `info` by default, `debug` for every request, `trace` for everything and `warn` for problems only. It takes the syntax of `RUST_LOG`, such as `warn,fair_witness_core=debug`. At every level, it leaves out the folder watcher's events below `warn`, one for each file event, and everything the AI client logs, because the client quotes the AI API's responses and those can repeat the API key. The events log has what the model answered.

The log holds everything the AI read with its tools, which can be personal data from your research. Like `credentials.toml`, its files are readable only by you on macOS and Linux. Delete the logs you no longer need.

A `.gitignore` inside `.fair-witness/` keeps `credentials.toml`, `madmp.json`, the `.part` files and `log/` out of git. You can run `fair-witness` from any subfolder. It finds `.fair-witness/` by looking upwards.

### What is ignored

Some files in a project are not research data, and checking them would only cost time and tokens. `run` does not report changes to them, and the AI cannot read them:

- `.fair-witness/`, also in a subfolder, such as one that `setup` made there or that came with a colleague's folder
- `.git`, so a commit does not count as a change
- folders tools fill on their own: `.venv/`, `venv/`, `__pycache__/`, `node_modules/` and `.ipynb_checkpoints/`
- folders a tool tagged as a cache with a [`CACHEDIR.TAG`](https://bford.info/cachedir/) file, as cargo tags `target/` and pytest, mypy and ruff tag their `.pytest_cache/`, `.mypy_cache/` and `.ruff_cache/`
- editor swap and backup files (`*.swp`, `*.swo`, `*.swx`, `*~`, `.#*`, `#*#`) and `.DS_Store`
- the locks LibreOffice and Microsoft Office keep next to an open document (`.~lock.*#`, `~$*`)
- downloads that are not finished yet, from browsers (`*.part`, `*.crdownload`) and from the ownCloud and Nextcloud clients (`.*.~*`), so a download shows up only under its final name
- the journal and logs of the ownCloud and Nextcloud clients (`.sync_*.db*`, `._sync_*.db*`, `.csync_journal.db*`, `.owncloudsync.log*`, `.nextcloudsync.log*`, `.nextcloudpermissions.log*`)

A folder synced with ownCloud or Nextcloud works like any other. The client uploads `.fair-witness/` with everything else, though, so anyone the folder is shared with would get your DAMAP session and API key from `credentials.toml`. Before you run `setup` in a synced folder, add `credentials.toml` to the client's ignored files. Ignoring it later leaves the uploaded copy on the server, so delete that copy, replace the API key with a new one and run `fair-witness logout` to end the DAMAP session in the copy. Then run `setup` to log in again.

To ignore more, edit `.fair-witness/ignore`, which `setup` creates with a commented template. It uses the same syntax as a `.gitignore`. Paths are relative to the project root, and a line starting with `!` undoes a rule, including a built-in one or a folder's cache tag. A `.fair-witness/` folder is always skipped, wherever it is:

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

About five seconds later, the changes show up as one numbered **change set**:

```
created: data                                                    #1
created: data/patients.csv                                       #1
  ↳ checking against the DMP… 4s
```

Changes less than two seconds apart go into one set. In a folder that never stays quiet that long, such as during a long copy, a set ends a minute after its first change. Writes to a file that a program keeps open, such as a log, show up only once they pause for two seconds. `created`, `changed`, `renamed` and `deleted` are colored so you can tell them apart. Files that are not research data are left out, as described in [What is ignored](#what-is-ignored).

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

Changes you make while the AI checks a set show up right away as sets of their own, and the AI checks them in turn, each verdict under its own set. A file that is gone by the time the AI gets to its set stays listed, and the AI is told that it can no longer be read.

To check its verdict, the AI only gets the DMP and the list of changed paths. When the names alone are not enough, it can list folders and read the first 16 KB of files. It cannot look outside the project folder or at what `run` ignores, such as `.fair-witness/`, as described in [What is ignored](#what-is-ignored).

What you do about a contradiction is up to you. Either undo the change, or, if the plan has changed, update the DMP in DAMAP and restart `run` so it picks up the new version.

### 4. See what the AI did

Press `d` to open the **debug pane** next to the change list. It shows each review step by step: the prompt with the DMP, every request to the model, the tools it called with their results, its reasoning (if the model shares it), the token counts, how long each reply took and how long a request waited when the API had too many.

- `Tab` switches which pane the arrow keys scroll.
- `c` collapses or expands long blocks, such as the DMP JSON.
- `d` hides the pane again.

This is the place to look when a verdict seems wrong. The log in `.fair-witness/log/` holds the same steps, so a review can still be read after `run` quits.

### 5. Switch the model

Press `m` to change the model without restarting:

- Type to filter the model list, and use `↑↓` to choose.
- `Tab` switches to the **effort** column for models that support reasoning effort: `default`, `none`, `minimal`, `low`, `medium`, `high`, `xhigh` and `max`. `default` sends no effort, so the model uses its own, and `none` asks for no reasoning, which some models ignore. Each model takes only some of these names, and its API refuses, ignores or rounds the others. The list also holds an effort given with `--effort` that it does not offer.
- `Enter` first asks the API once whether it takes the model and effort, as `run` does when it starts. If the API refuses them, the panel stays open with its message, which often names the efforts the model takes. Otherwise the choice is used from the next change set on and saved to `config.toml`, unless `--ai-url` named another API than the one chosen in `setup`. When the check cannot tell, such as when the API answers `429 Too Many Requests`, the Changes pane says so. `Esc` cancels, also while the panel checks.

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
| `fair-witness logout` | End the DAMAP session and forget the saved login for this folder |

`logout` asks DAMAP's login server to end the session before it forgets the login, so a copy of `credentials.toml` stops working too. When DAMAP or its login server cannot be reached, or refuses, `logout` forgets the login anyway and says that the session stays valid until it expires.

If `run` finds no DMP or no model, it still watches and lists changes, but does not check them. The header says so.

Before it watches, `run` asks the API once whether it takes the model and effort, with a request like the first of a review but cut off after one token. If the API refuses them, `run` stops with the API's message, which often names what the model takes, instead of failing every review later. If the API cannot be reached, answers that it is busy with `408 Request Timeout`, `409 Conflict` or `429 Too Many Requests`, or fails on its side, `run` starts anyway, and the header warns that the model could not be checked.

If DAMAP cannot be reached, `run` checks changes against the copy of the DMP it saved last time, in `.fair-witness/madmp.json`, and the header warns about it. This works only for the DMP chosen in `setup`, also when `--dmp` names it, and not for another DMP given with `--dmp`.

### Running from a script

Scripts and agents can run fair-witness without a terminal and without DAMAP. These options of `run` work with or without `--headless`:

| Flag | Environment variable | |
|---|---|---|
| `--headless` | | Print each event as one line of JSON on stdout instead of drawing the screen |
| `--project <dir>` | | Run in this folder instead of the current one. As there, the project is the nearest folder upwards that is set up, or this folder itself if none is |
| `--dmp-file <path>` | | Check against the maDMP JSON in this file instead of downloading the DMP from DAMAP |
| `--ai-url` | `FAIR_WITNESS_AI_URL` | Base URL of the OpenAI-compatible API, instead of the one chosen in `setup` |
| `--api-key` | `FAIR_WITNESS_API_KEY` | API key for it |
| `--model` | `FAIR_WITNESS_MODEL` | Model to use, instead of the one chosen in `setup` |
| `--effort` | `FAIR_WITNESS_EFFORT` | How much the model reasons, instead of the effort chosen with `m`: a name the model takes, such as `low` or `xhigh`, which goes to the API as given. `default` sends no effort, so the model uses its own, and `none` asks for no reasoning, which some models ignore |
| | `FAIR_WITNESS_DEBOUNCE_MS` | How long the folder has to be quiet before its changes count as one set, in milliseconds (default 2000). A set ends 30 times as long after its first change at the latest |

With `--dmp-file` and the AI flags, the folder does not need to be set up, and nothing is written into it:

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

Each line is the same as in the events log of `.fair-witness/log/`: the time, the event and the number of the change set it belongs to. `trace` events are the steps the debug pane shows, and a set ends with a `verdict` or a `review_failed`. The `changes` of the next sets can come before that, while the set is still reviewed, since the sets are reviewed one after another. Paths are relative to the project folder and written as the Changes pane and the AI see them, so that every name fits on one line and none is lost: `/` between folders on every system, a backslash in a name as `\\`, line breaks and other control characters as `\n`, `\r`, `\t` or `\u{1b}`, and a byte that is not UTF-8 as `\xe9`. JSON then doubles each backslash, as in `{"created":"latin1-\\xe9t\\xe9.csv"}`. Messages for people, such as which DMP is used, go to stderr. A folder that is set up also gets its log, and one that never ran `setup` gets none.

`--headless` stops when stdin closes, on `Ctrl+C` or on SIGTERM, also while it is still starting, and exits with 0. If stdout is not read, so that it cannot write out the events it has within 2 seconds, it exits with 1 instead. It never asks you to log in to DAMAP: when the saved login has expired, it falls back to the saved copy of the DMP, or fails if there is none. It exits with 2 when it cannot start, such as in a folder that is not set up or with a model or effort the API refuses, and with 1 when it fails while watching.

The API key goes only to the API chosen in `setup`. With another `--ai-url`, pass its key with `--api-key`. A model chosen with `m` for such an API is used until you quit but not saved. As in `setup`, a key goes over plain `http://` only to this machine, and `run` exits with 2 before it sends anything when one would go elsewhere.

## Troubleshooting

- **"this folder is not set up yet"**: run `fair-witness setup` in the folder, or in one of its parent folders.
- **"Is DAMAP running?"**: the saved DAMAP URL can't be reached. Start DAMAP, or run `setup` to change the URL.
- **"… does not look like an OpenAI-compatible API"**: the AI URL usually needs to end in `/v1`.
- **"… which would send the DAMAP login unencrypted"**: the DAMAP URL uses plain HTTP to another machine. Run `setup` with DAMAP's `https://` address, also for a URL saved by an earlier `setup`.
- **"DAMAP's login server … uses plain HTTP"**: DAMAP logs in through a login server on another machine over plain HTTP. Only the people who run that DAMAP can change it.
- **"… which would send the API key unencrypted"**: the API key would cross the network unencrypted. Use `https://`, reach the API at `http://localhost` through an SSH tunnel, or leave out the key if the API needs none. For an API chosen in an earlier `setup`, run `setup` again.
- **"login failed: … answered …"**: DAMAP's login server refused the login, and its reason follows the status, such as "Invalid user credentials" for a wrong username or password. With only a status, such as `502 Bad Gateway`, the login server or a proxy in front of it failed. Try again later.
- **"answered 401 Unauthorized"** from the AI API: the API key is missing or wrong. Aqueduct always needs one.
- **"offers no models"**: with Ollama, install a model first, for example `ollama pull qwen3:8b`; otherwise pass `--model`.
- **"… refuses your-model (high effort)"**: the API does not take the model at this effort, and its message after the colon says why, such as which efforts the model takes. Pass another with `--effort`, or `--effort default` to send none, and save it with `m` and `Enter`. Or choose another model with `setup`.
- **"429 Too Many Requests"**: the API limits how many requests a model gets a minute, and a review takes 2 or 3. fair-witness waits as long as the API asks, at most a minute each time, and sends the request again, up to 10 times. The review fails when the API keeps refusing or asks for a longer wait, as for a daily quota. Wait and change the files again, or pick a model with a higher limit with `m`.
- **"review failed"**: open the debug pane (`d`) to see the last request and the error. Small models sometimes don't call tools properly; try a larger one with `m`.
- **Something else went wrong**: run `FAIR_WITNESS_LOG=debug fair-witness run` and read the newest `-diagnostics.log` in `.fair-witness/log/`.
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
mise run scenarios      # run the scripted researcher sessions in scenarios/edge-cases/ and scenarios/realistic/
mise run check          # everything CI checks: formatting, clippy, tests, scenarios
```

Clippy warnings fail the check.

The code is split into three crates in `crates/`. `fair-witness-core` is a library that watches the folder, talks to DAMAP and reviews changes, without any terminal code. `fair-witness` is the binary with the commands, the setup questions and the full-screen interface. `fair-witness-scenarios` runs the scenarios in `scenarios/` against the binary, with a fake model or a real one, and is not released. [scenarios/README.md](scenarios/README.md) describes them.

## Releasing

A push to `main` publishes a release once `mise run check` passes on it, if `main` differs from the latest release in `crates/` outside `crates/fair-witness-scenarios/`, `Cargo.toml`, `Cargo.lock`, `dist-workspace.toml` or `.github/workflows/release.yml`. Changes to anything else, such as `.github/workflows/ci.yml`, this README, `mise.toml` or the scenario runner, would build the same binaries and are not released. The runner shares `Cargo.lock` with the binary, so a change to its dependencies that touches `Cargo.lock` still releases. The `publish` job in `.github/workflows/ci.yml` tags the next version (the `version` in `crates/fair-witness/Cargo.toml`, or the next patch if that is already released) and starts `release.yml`, which builds the binaries and installers for Linux, macOS and Windows and attaches them to a GitHub release. To start a new minor or major line, bump `version` in `crates/fair-witness/Cargo.toml`.

The release workflow is generated by [dist](https://github.com/axodotdev/cargo-dist); after changing `dist-workspace.toml`, run `dist generate`.
