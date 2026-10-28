# Usage

fair-witness watches a project folder and flags changes that contradict its data management plan (DMP) in [DAMAP](https://github.com/tuwien-csd/damap-backend). Each time files in the folder are created, changed, renamed or deleted, an AI model compares them with the DMP and tells you whether they are in line with it, for example when patient data lands in a project whose DMP says it holds no personal data.

## What you need

- **A DAMAP instance** you can log in to, with at least one DMP. The default is TU Wien's, [damap.it.tuwien.ac.at](https://damap.it.tuwien.ac.at). To run one locally, see [Running DAMAP locally](configuration.md#running-damap-locally).
- **An AI model behind an OpenAI-compatible API.** The default is TU Wien's [Aqueduct](https://aqueduct.ai.datalab.tuwien.ac.at) gateway (`https://aqueduct.ai.datalab.tuwien.ac.at/v1`). Create an API token in its web interface. A local [Ollama](https://ollama.com) (`http://localhost:11434/v1`), OpenAI, or anything else that serves `/v1/models` and `/v1/chat/completions` with tool calling works too.

The [README](../README.md#install) shows how to install fair-witness.

## Set up a folder

Go to the folder you want to watch and run `setup`:

```sh
cd ~/research/my-project
fair-witness setup
```

It asks four things, in order:

1. **DAMAP URL.** The address you open DAMAP at in the browser (default `https://damap.it.tuwien.ac.at`).
2. **Login.** A browser window opens on DAMAP's login page. Confirm the code shown in the terminal. If DAMAP's login server does not allow browser login, as at TU Wien, you're asked for your username and password instead.
3. **DMP.** Pick the DMP that describes this folder. Type to filter the list.
4. **AI model.** Enter the API's URL (default Aqueduct) and, if it needs one, an API key. Aqueduct does. Then pick a model from the ones the API offers.

```
$ fair-witness setup
DAMAP URL (as opened in the browser) [https://damap.it.tuwien.ac.at]:
Connected to DAMAP Tool at https://damap.it.tuwien.ac.at
Not logged in.
Browser login is not enabled for Keycloak client "damap"; using username and password instead.
Username: you
Password: [hidden]
DMP for this folder (type to filter): #3 Soil samples 2026, changed 2026-09-30
AI API URL (OpenAI-compatible) [https://aqueduct.ai.datalab.tuwien.ac.at/v1]:
API key (empty for none): [hidden]
Model (type to filter): your-model
Changes will be checked with your-model.
Run `fair-witness run` to start watching this folder.
Edit /home/you/research/my-project/.fair-witness/ignore to choose which paths to ignore.
```

With browser login, the login lines are instead:

```
To log in, open https://damap.example.org/auth/realms/damap/device?user_code=ABCD-EFGH
and confirm the code ABCD-EFGH
```

Run `setup` again any time to switch DMP, model or DAMAP instance. It remembers your previous answers and offers to keep you logged in. [Running setup again](configuration.md#running-setup-again) explains what it keeps, and [Setting up without prompts](configuration.md#setting-up-without-prompts) how to pass the answers as flags.

`setup` saves its answers in a `.fair-witness/` folder at the root of the project, as described in [What setup saves](configuration.md#what-setup-saves). Some files are left out of checks, as described in [What is ignored](configuration.md#what-is-ignored).

## Watching a folder

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
  ⠋ checking against the DMP… 4s
```

Changes less than two seconds apart go into one set. In a folder that never stays quiet that long, such as during a long copy, a set ends a minute after its first change. Writes to a file that a program keeps open, such as a log, show up only once they pause for two seconds. `created`, `changed`, `renamed` and `deleted` are colored so you can tell them apart. Files that are not research data are left out, as described in [What is ignored](configuration.md#what-is-ignored).

A set of more than 50 changes is listed per folder instead, for you and for the AI, such as `created: 412 paths under data/raw/`. The AI can still ask for every changed path, 200 at a time.

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

To check its verdict, the AI only gets the DMP and the list of changed paths. When the names alone are not enough, it can list folders, 200 entries at a time, and read the first 16 KB of files. It cannot look outside the project folder or at what `run` ignores, such as `.fair-witness/`, as described in [What is ignored](configuration.md#what-is-ignored).

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

`run` also stops watching when the project folder or a folder above it is moved, renamed, deleted or replaced, and says so in the header and the Changes pane. Reviews still running or waiting are dropped, since they could no longer read the folder. To watch it again, start `fair-witness run` where the folder is now, or once a deleted folder is back.

## Keys

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
| `fair-witness run --headless` | Print what happens as lines of JSON instead of drawing the screen, see [Running from a script](scripting.md) |
| `fair-witness logout` | End the DAMAP session and forget the saved login for this folder |

`fair-witness <command> --help` lists every option of a command.

`logout` asks the login server of the DAMAP the session was saved for to end it before it forgets the login, also when `config.toml` names another DAMAP since, so a copy of `credentials.toml` with the same login stops working too. A copy from before a later login holds another session, which `logout` cannot end. When DAMAP or its login server cannot be reached, or refuses, `logout` forgets the login anyway and says that the session stays valid until it expires.

If `run` finds no DMP or no model, it still watches and lists changes, but does not check them. The header says so.

Before it watches, `run` asks the API once whether it takes the model and effort, with a request like the first of a review but cut off after one token. If the API refuses them, `run` stops with the API's message, which often names what the model takes, instead of failing every review later. If the API cannot be reached, answers that it is busy with `408 Request Timeout`, `409 Conflict` or `429 Too Many Requests`, or fails on its side, `run` starts anyway, and the header warns that the model could not be checked.

If DAMAP cannot be reached, `run` checks changes against the copy of the DMP it saved last time, in `.fair-witness/madmp.json`, and the header warns about it. This works only for the DMP chosen in `setup`, also when `--dmp` names it, and not for another DMP given with `--dmp`.
