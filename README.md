# damap-helper

Watches a project folder and flags changes that contradict its data management plan (DMP) in [DAMAP](https://github.com/tuwien-csd/damap-backend).

Each time files in the folder are created, changed, renamed or deleted, an AI model compares them with the DMP and tells you whether they are in line with it, for example when patient data lands in a project whose DMP says it holds no personal data.

## What you need

- **A DAMAP instance** you can log in to, with at least one DMP. The default is TU Wien's, [damap.it.tuwien.ac.at](https://damap.it.tuwien.ac.at). To run one locally, see [Running DAMAP locally](#running-damap-locally).
- **An AI model behind an OpenAI-compatible API.** The default is TU Wien's [Aqueduct](https://aqueduct.ai.datalab.tuwien.ac.at) gateway (`https://aqueduct.ai.datalab.tuwien.ac.at/v1`); create an API token in its web interface. A local [Ollama](https://ollama.com) (`http://localhost:11434/v1`), OpenAI, or anything else that serves `/v1/models` and `/v1/chat/completions` with tool calling works too.

## Install

macOS and Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/vlad-dancea/damap-helper/releases/latest/download/damap-helper-installer.sh | sh
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/vlad-dancea/damap-helper/releases/latest/download/damap-helper-installer.ps1 | iex"
```

Check that it works with `damap-helper --version`.

## Set up a folder

Go to the folder you want to watch and run `setup`:

```sh
cd ~/research/my-project
damap-helper setup
```

It asks four things, in order:

1. **DAMAP URL.** The address you open DAMAP at in the browser (default `https://damap.it.tuwien.ac.at`).
2. **Login.** A browser window opens on DAMAP's login page; confirm the code shown in the terminal. If DAMAP's login server does not allow browser login, you're asked for your username and password instead.
3. **DMP.** Pick the DMP that describes this folder. Type to filter the list.
4. **AI model.** Enter the API's URL (default Aqueduct) and, if it needs one, an API key. Aqueduct does. Then pick a model from the ones the API offers.

```
$ damap-helper setup
DAMAP URL (as opened in the browser) [https://damap.it.tuwien.ac.at]:
Connected to DAMAP Tool at https://damap.it.tuwien.ac.at
To log in, open https://damap.it.tuwien.ac.at/auth/realms/damap/device?user_code=ABCD-EFGH
and confirm the code ABCD-EFGH
DMP for this folder (type to filter): #3 Soil samples 2026, changed 2026-09-30
AI API URL (OpenAI-compatible) [https://aqueduct.ai.datalab.tuwien.ac.at/v1]:
API key (empty for none): ********
Model (type to filter): your-model
Changes will be checked with your-model.
Run `damap-helper run` to start watching this folder.
```

Run `setup` again any time to switch DMP, model or DAMAP instance. It remembers your previous answers and offers to keep you logged in.

### Setting up without prompts

The DAMAP URL and the AI settings can also be passed as flags or environment variables. That's handy for scripts or for keeping an API key out of your shell history:

| Flag | Environment variable | |
|---|---|---|
| `--url` | | DAMAP URL |
| `--ai-url` | `DAMAP_HELPER_AI_URL` | Base URL of the OpenAI-compatible API |
| `--api-key` | `DAMAP_HELPER_API_KEY` | API key for it |
| `--model` | `DAMAP_HELPER_MODEL` | Model to use |

```sh
DAMAP_HELPER_API_KEY=sk-... damap-helper setup --url https://damap.example.org \
  --ai-url https://api.openai.com/v1 --model gpt-5-mini
```

You still log in and pick the DMP interactively.

### What setup saves

Everything goes into a `.damap-helper/` folder at the root of the project:

- `config.toml`: DAMAP URL, DMP id, AI URL, model and effort. Safe to commit.
- `credentials.toml`: your DAMAP session and the API key. On macOS and Linux, readable only by you.
- `madmp.json`: a copy of the DMP, refreshed on every `run`.

A `.gitignore` inside it keeps the last two out of git. You can run `damap-helper` from any subfolder; it finds `.damap-helper/` by looking upwards.

## Tutorial: watching a folder

### 1. Start watching

```sh
damap-helper run
```

The DMP is downloaded and the screen opens:

```
╭ damap-helper ─────────────────────────────────────────────────────╮
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

Changes that happen close together are grouped into one set. `created`, `changed`, `renamed` and `deleted` are colored so you can tell them apart. Changes inside `.damap-helper/` are ignored.

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

To check its verdict, the AI only gets the DMP and the list of changed paths. When the names alone are not enough, it can list folders and read the first 16 KB of files. It cannot look outside the project folder or into `.damap-helper/`.

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
- `Enter` uses the choice from the next change set on and saves it to `config.toml`. `Esc` cancels.

To use a different API or key, quit and run `damap-helper setup`.

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
| `damap-helper setup` | Set up the current folder, or change its DAMAP login, DMP or model |
| `damap-helper run` | Watch the folder and check changes against the DMP |
| `damap-helper run --dmp <id>` | Check against another DMP this time only, without changing the saved choice |
| `damap-helper logout` | Forget the saved DAMAP login for this folder |

If `run` finds no DMP or no model, it still watches and lists changes, but does not check them. The header says so.

## Troubleshooting

- **"this folder is not set up yet"**: run `damap-helper setup` in the folder, or in one of its parent folders.
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

Then run `damap-helper setup --url http://localhost:8085` and log in as `user` with password `user`.

To build `damap-helper` from source: `cargo run -p damap-helper -- setup`. With [mise](https://mise.jdx.dev), `mise install` sets up the Rust toolchain.

## Releasing

Every push to `main` publishes a release. `.github/workflows/publish.yml` tags the next version (the `version` in `damap-helper/Cargo.toml`, or the next patch if that is already released) and starts `release.yml`, which builds the binaries and installers for Linux, macOS and Windows and attaches them to a GitHub release. To start a new minor or major line, bump `version` in `damap-helper/Cargo.toml`.

The release workflow is generated by [dist](https://github.com/axodotdev/cargo-dist); after changing `dist-workspace.toml`, run `dist generate`.
