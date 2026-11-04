# Portal dev loop

```bash
docker compose -f dev/docker-compose.yml --profile mock up -d
./scripts/fetch-tools.sh && ./scripts/build-css.sh watch   # separate terminal
PORTAL_CONFIG=dev/config.yaml cargo run
```

Then open http://localhost:8088.

- `docker compose --profile mock up -d` starts mosquitto (`:1883`) and the
  mock publisher (`dev/mock/`, one simulated Hub — see
  `dev/mock/README.md`). Drop `--profile mock` to run against a real device
  tunneled to `localhost:1883` instead.
- `make multi-up` (`--profile multi`) starts the demo cluster instead: the
  server Hub plus two client Hubs, one with P2P Nodes and one with no
  concentrator and every system alert tripped. The portal needs no extra
  config — it keys one device per hostname. `dev/hub.sh list` names them,
  `dev/mock/README.md` says what each covers.
- `dev/config.yaml` binds `127.0.0.1:8088`, points MQTT at `localhost:1883`,
  and uses `./dev-data` for storage — see `src/config.rs`.
- Template edits hot-reload (minijinja loader in debug); Rust changes need
  `cargo check`/`cargo run` again.
- Logs go to stderr at `info` by default; `RUST_LOG=debug` (or
  `RUST_LOG=info,portal=debug`) for more. Under `make up` stderr is appended
  to `dev-data/portal.log` (`make logs` follows it). If the process exits
  straight back to the prompt, read the last line — the usual cause is
  `127.0.0.1:8088` already being held by an earlier instance, which `make up`
  now reports by name instead of letting the bind fail (`lsof -nP -iTCP:8088
  -sTCP:LISTEN` to see it by hand).
- `make up`/`make down` are the detached loop: the PID file holds the server
  binary's own PID, not cargo's, because `cargo run` forks the binary as a
  child and killing cargo leaves it listening. `make down` falls back to
  finding it by port when the PID file is missing.

## Notifications

`make up` also starts two catchers, so alarm e-mails and webhooks can be tried
end to end without a real mailbox or endpoint:

- **Mailpit** takes the mail. Settings → Notifications → E-mail: provider
  SMTP, host `localhost`, port `1025`, TLS and SSL off, no login, any sender
  address. The inbox is http://localhost:8025.
- **webhook-echo** (`dev/webhook_echo.py`, stdlib only) takes the webhook.
  URL `http://localhost:8099/alarms`; http://localhost:8099/ lists the last 50
  requests, newest first, with their JSON bodies.

Then give the Hub a recipient under "Upozornění zařízení" and walk one alarm
through its life: `make alarm SEV=warning` opens it, `make alarm SEV=critical`
escalates it (one "escalated" message, the first time only), `make
alarm-clear` ends it. Every attempt, sent or failed, is also in the history
table at the bottom of the page.
