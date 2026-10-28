# Configuration

`fair-witness setup` asks for its settings, as described in [Set up a folder](usage.md#set-up-a-folder), and saves them in the project's `.fair-witness/` folder.

## Setting up without prompts

The DAMAP URL and the AI settings can also be passed as flags or environment variables. That's handy for scripts or for keeping an API key out of your shell history:

| Flag | Environment variable | |
|---|---|---|
| `--url` | | DAMAP URL |
| `--ai-url` | `FAIR_WITNESS_AI_URL` | Base URL of the OpenAI-compatible API |
| `--api-key` | `FAIR_WITNESS_API_KEY` | API key for the API at `--ai-url`, or empty to remove the saved one. Needs `--ai-url` |
| `--model` | `FAIR_WITNESS_MODEL` | Model to use |

```sh
read -rs key   # paste the key, kept out of the history
FAIR_WITNESS_API_KEY=$key fair-witness setup --url https://damap.example.org \
  --ai-url https://api.openai.com/v1 --model gpt-5-mini
```

You still log in and pick the DMP interactively. A key from `--api-key` or `FAIR_WITNESS_API_KEY` needs `--ai-url` or `FAIR_WITNESS_AI_URL` beside it, in `setup` and in `run`, so that a key exported for one API never goes to another one.

## Running setup again

`setup` remembers your previous answers and offers to keep you logged in. It saves the login as soon as you log in and the other answers only once you have given them all, so a `setup` you stop with `Ctrl+C` leaves `config.toml` as it was. After a switch to another DAMAP instance it offers no DMP from before, since each instance numbers its DMPs on its own.

The saved API key goes only to the API it was saved for. For that API, an empty answer keeps the key, and so does `--ai-url` with the same URL and no `--api-key`. Another API gets only a key you give for it, and the saved one is replaced, or removed when you give none. To remove the saved key and keep the API, run `fair-witness setup --ai-url <its URL> --api-key ''`. It removes the key before it contacts the API, so the key is gone even when an API that needs one then refuses to answer.

The effort chosen with `m` stays only when you keep the same API and model, because an effort one model takes can fail every review on another. When `setup` drops it, the line that names the model says so, and the model uses its default effort until you choose one with `m` in `run`.

Last, `setup` asks the API once whether it takes the model and the effort it keeps, as `run` does when it starts. If the API refuses the effort, `setup` shows the API's message, drops the effort and checks the model at its default effort. If the API refuses the model, `setup` shows the message and asks for another model, or stops when `--model` named it.

## Plain HTTP

fair-witness talks to DAMAP over plain `http://` only on this machine: `localhost`, `127.0.0.0/8` and `[::1]`, such as a DAMAP [running locally](#running-damap-locally) at `http://localhost:8085`. Anywhere else the login would cross the network unencrypted, and anyone along the way could read it and act as you in DAMAP. With the username and password login, that includes your password. `setup` and `run` refuse such a DAMAP URL before they send anything to it, and the same for DAMAP's login server, whose address comes from DAMAP. They reach a DAMAP or login server on this machine directly, also when `HTTP_PROXY` or `ALL_PROXY` names a proxy.

An API key goes over plain `http://` only to this machine: `localhost`, `127.0.0.0/8` and `[::1]`. Anywhere else it would cross the network unencrypted, where anyone along the way could read it and use it. fair-witness reaches an API on this machine directly, also when `HTTP_PROXY` or `ALL_PROXY` names a proxy, which would get the key the same way. For an API on another machine, use `https://`, or reach it at `http://localhost` through an SSH tunnel, such as `ssh -L 11434:localhost:11434 lab-machine` for Ollama on a lab machine. For a plain `http://` address on another machine, `setup` neither asks for a key nor sends the saved one, so an API that needs no key still works, and it refuses a key given with `--api-key`.

## What setup saves

Everything goes into a `.fair-witness/` folder at the root of the project:

- `config.toml`: DAMAP URL, DMP id, AI URL, model and effort. Safe to commit. `setup` writes it, and `run` only when you choose a model with `m`.
- `credentials.toml`: your DAMAP session and the API key, each with the servers it was saved for. fair-witness sends each only to those, so a URL changed in `config.toml`, such as by a collaborator's commit, gets neither: `run` asks you to log in again, or warns that it sends no API key. On macOS and Linux, readable only by you. A file saved by an earlier version has no servers in it, so the first command that reads it takes them from `config.toml` and saves them there. A secret whose server `config.toml` does not name, such as an API key without an AI URL, is dropped, and `setup` asks for it again.
- `madmp.json`: a copy of the DMP chosen in `setup`, with the DAMAP it came from, refreshed on every `run`. When DAMAP cannot be reached, `run` checks against this copy and warns that it does. It names the DMP's contributors with their e-mail addresses, so on macOS and Linux it is readable only by you.
- `ignore`: a commented template for paths to leave out of checks. `setup` creates it if it is missing and leaves an existing file unchanged.
- `<file>.<random>.part`: what was left when fair-witness stopped while saving `<file>`. `config.toml`, `credentials.toml`, `madmp.json` and the `.gitignore` are written under such a name first and then renamed over the old file, so a save that fails keeps the old file whole. You can delete these.
- `log/`: what each `run` did, written by `run`. `<start time>-events.jsonl` has one line of JSON for every change set, every step of its review and the verdict, with the time, the model and effort, the token counts and how long each reply took. To keep it short, the system prompt, the DMP and the model are written once, before the first review, and again only when they change. `<start time>-diagnostics.log` has what happened outside the reviews, such as requests to DAMAP, folders that could not be read and notifications that failed. The `FAIR_WITNESS_LOG` environment variable sets how much it holds: `info` by default, `debug` for every request, `trace` for everything and `warn` for problems only. It takes the syntax of `RUST_LOG`, such as `warn,fair_witness_core=debug`. At every level, it leaves out the folder watcher's events below `warn`, one for each file event, and everything the AI client logs, because the client quotes the AI API's responses and those can repeat the API key. The events log has what the model answered.

The log holds everything the AI read with its tools, which can be personal data from your research. Like `credentials.toml`, its files are readable only by you on macOS and Linux. Delete the logs you no longer need.

A `.gitignore` inside `.fair-witness/` keeps `credentials.toml`, `madmp.json`, the `.part` files and `log/` out of git. You can run `fair-witness` from any subfolder. It finds `.fair-witness/` by looking upwards.

## What is ignored

Some files in a project are not research data, and checking them would only cost time and tokens. `run` does not report changes to them, and the AI cannot read them:

- `.fair-witness/`, also in a subfolder, such as one that `setup` made there or that came with a colleague's folder
- `.git`, so a commit does not count as a change
- folders tools fill on their own: `.venv/`, `venv/`, `__pycache__/`, `node_modules/` and `.ipynb_checkpoints/`
- folders a tool tagged as a cache with a [`CACHEDIR.TAG`](https://bford.info/cachedir/) file, as cargo tags `target/` and pytest, mypy and ruff tag their `.pytest_cache/`, `.mypy_cache/` and `.ruff_cache/`
- editor swap and backup files (`*.swp`, `*.swo`, `*.swx`, `*~`, `.#*`, `#*#`) and `.DS_Store`
- the locks LibreOffice and Microsoft Office keep next to an open document (`.~lock.*#`, `~$*`)
- downloads that are not finished yet, from browsers (`*.part`, `*.crdownload`) and from the ownCloud and Nextcloud clients (`.*.~*`), so a download shows up only under its final name
- the journal and logs of the ownCloud and Nextcloud clients (`.sync_*.db*`, `._sync_*.db*`, `.csync_journal.db*`, `.owncloudsync.log*`, `.nextcloudsync.log*`, `.nextcloudpermissions.log*`)
- what a symbolic link leads to, such as a share linked into the project to reach large data without copying it. Only the link itself counts as a file.

A folder synced with ownCloud or Nextcloud works like any other. The client uploads `.fair-witness/` with everything else, though, so anyone the folder is shared with would get your DAMAP session and API key from `credentials.toml`. Before you run `setup` in a synced folder, add `credentials.toml` to the client's ignored files. Ignoring it later leaves the uploaded copy on the server, so delete that copy, replace the API key with a new one and run `fair-witness logout`, which ends the DAMAP session in the copy as long as you have not logged in again since the client last uploaded it. Then run `setup` to log in again.

To ignore more, edit `.fair-witness/ignore`, which `setup` creates with a commented template. It uses the same syntax as a `.gitignore`. Paths are relative to the project root, and a line starting with `!` undoes a rule, including a built-in one or a folder's cache tag. A `.fair-witness/` folder is always skipped, wherever it is:

```gitignore
# scratch output of the analysis scripts
scratch/
*.log
# this project keeps data in a folder called venv
!venv/
```

The file is read when `run` starts, so restart it after a change. The project's own `.gitignore` is not used, because data kept out of git is often exactly the data the DMP is about.

## Running DAMAP locally

With Docker installed, start DAMAP's development setup from a clone of [damap-backend](https://github.com/tuwien-csd/damap-backend):

```sh
git clone https://github.com/tuwien-csd/damap-backend
cd damap-backend
docker compose up -d --wait   # DAMAP at http://localhost:8085
docker compose down           # to stop it
```

Then run `fair-witness setup --url http://localhost:8085` and log in as `user` with password `user`.
