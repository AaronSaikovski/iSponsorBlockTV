# Repository Guidelines

## Project Overview

iSponsorBlockTV is a self-hosted Python companion for YouTube TV apps. It uses
SponsorBlock segments to skip sponsored content and can mute/skip ads and control
autoplay. Device pairing and configuration have Textual and command-line interfaces.

## Architecture & Data Flow

- `src/main.py` and the installed console command enter `helpers.py`, where Click
  dispatches setup or validates configuration and starts the service.
- `main.py` owns the asyncio lifecycle, shared aiohttp session, and one
  `DeviceListener` per configured device. Configuration, API helpers, sessions,
  and loggers are passed explicitly; preserve this dependency-passing pattern.
- `ytlounge.py` adapts pyytlounge. Lounge events update mutable playback state and
  notify the listener, which replaces its pending playback task. `api_helpers.py`
  fetches/caches SponsorBlock segments; the listener schedules seeks using elapsed
  time, playback speed, and device offset, then optionally reports viewed segments.
- Setup edits `Config` directly. Runtime validation converts device dictionaries
  into `Device` objects; keep setup/persistence separate from validated runtime state.
- Discovery combines DIAL/SSDP and Chromecast results, deduplicating screen IDs.
  Normal operation need not share the device LAN; automatic discovery does.

## Key Directories

- `src/iSponsorBlockTV/`: service, configuration, integrations, setup UI, and its
  adjacent `setup-wizard-style.tcss` stylesheet.
- `src/`: thin executable wrappers; prefer `src/main.py ... setup` over the separate
  `main_tui.py`, whose config argument mismatches `Config`'s directory API.
- `.github/workflows/`: container builds, PyApp releases, and Docker Hub docs.
  There is no dedicated tests or scripts directory.

## Development Commands

Run from the repository root with Python 3.11 or newer:

```sh
python3 -m venv .venv
source .venv/bin/activate
python -m pip install -e .
python -m pip install hatch pre-commit

python src/main.py --data ./data setup       # Textual configurator
python src/main.py --data ./data setup-cli   # Terminal prompts
python src/main.py --data ./data --debug start

python -m hatch build                      # Wheel/sdist in dist/
pre-commit run --all-files                  # Includes modifying fixers
```

After installation, `iSponsorBlockTV` can replace `python src/main.py`. Put global
options before the subcommand; no subcommand defaults to `start`.
For a local container build, use `docker build -t isponsorblocktv .`.
`docker-compose.yml` uses a published image, not that local build; replace its
placeholder host data path before running `docker compose up -d`.

## Code Conventions & Common Patterns

- Follow Ruff formatting: four spaces, double quotes, 100-character lines.
  Use snake_case functions/attributes, PascalCase classes, and package-relative
  imports. Preserve upstream protocol keys such as `videoId` and `currentTime`.
- Keep network work asynchronous. Retain/cancel tasks explicitly and close resources
  at their owning lifecycle boundary. Chromecast bridges thread callbacks with
  `call_soon_threadsafe` and blocking calls with `asyncio.to_thread`.
- Use existing loggers for operational errors. Reconnect loops retry transient
  failures; some existing paths catch `BaseException`, so explicitly review
  cancellation behavior when changing exception handling.
- Keep Lounge commands serialized by the existing mutex: the adapter depends on
  pyytlounge private state and command offsets.
- Textual uses `compose`, `@on` handlers, reactive watchers, modal callbacks, and
  kebab-case widget IDs. Preserve cancellation and stale-result guards in setup.
- Preserve units and sentinel semantics: persisted device offsets are milliseconds,
  runtime offsets are seconds; absent categories default to sponsor, while `[]`
  disables segment fetching. Cached functions return `(value, ignore_ttl)` internally.

## Important Files

Paths below are relative to `src/iSponsorBlockTV/` unless rooted explicitly.

| File | Change here for |
| --- | --- |
| `__main__.py`, `helpers.py` | Entry point, CLI, config lifecycle |
| `main.py`, `ytlounge.py` | Device lifecycle, playback, Lounge protocol |
| `api_helpers.py`, `conditional_ttl_cache.py` | APIs, segments, caching |
| `setup_wizard.py`, `config_setup.py` | Textual and CLI configuration flows |
| `dial_client.py`, `chromecast_client.py` | Device discovery and pairing |
| `constants.py` | Service URLs, categories, client constants |
| Root `pyproject.toml`, `requirements.txt` | Packaging and dependency pins |
| Root `config.json.template`, `Dockerfile` | Config example and container |

## Runtime/Tooling Preferences

- Use Python/pip and Hatchling packaging. Runtime dependencies come from pinned
  `requirements.txt` via Hatch's requirements hook; keep that single source of truth.
  Release builds use Python 3.11; Docker uses Python 3.13 on Alpine. Rust/PyApp is
  release-binary tooling, not required for source development.
- `--data` selects a directory containing `config.json`, not a filename. Its fallback
  is `iSPBTV_data_dir`, then the platform user-data directory. Keep pairing/config
  data local. Docker persists it at `/app/data`.
- Generate configuration through setup: the template contains invalid placeholders.
  A nonempty channel whitelist requires a YouTube API key. Template ad toggles differ
  from missing-key defaults; consult `Config` before changing defaults.
- Consult the [installation wiki](https://github.com/dmunozv04/iSponsorBlockTV/wiki/Installation)
  for deployment and `README.md` for device compatibility and pairing caveats.

## Testing & QA

- No automated test suite, test runner, or coverage threshold is configured. Do not
  assume pytest or interpret generic test-related `.gitignore` entries as tooling.
- Pre-commit runs Ruff lint/format, Markdown lint, syntax and whitespace checks;
  several hooks modify files. GitHub workflows build/publish artifacts, not tests.
- For behavior changes, use a dedicated data directory and exercise the affected
  setup/playback path with a paired device and network access. Report what was
  actually verified; static checks cannot prove playback behavior.
- Prioritize category sentinel handling, offset conversion, segment merging/cache
  expiry, task replacement/cancellation, and discovery URL/IP validation when adding
  regression coverage. Active DIAL discovery can launch YouTube on real devices.
