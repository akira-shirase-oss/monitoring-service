# fixtures/

Deterministic failure fixtures for the monitor: healthy, slow and failing targets (the guide's
Node fault server lives here). The old implementation's fixtures are gone (D-001); these are
rebuilt from the contract in `02_Service_Monitor_Rebuild.docx`.

## fault-server.mjs

Zero-dependency Node fault server:

    node fixtures/fault-server.mjs [port]      # default 127.0.0.1:58700

| Route                     | Behaviour                                        |
| ------------------------- | ------------------------------------------------ |
| `/ok`                     | `200` immediately                                |
| `/fail`                   | `500` immediately                                |
| `/status/<n>`             | HTTP `<n>` (100–599) — use `301` to prove a probe does not follow redirects |
| `/slow?ms=<n>`            | `200` after `<n>` ms                             |
| `/flaky?fail=<f>&pass=<p>`| `<f>` consecutive `500`s, then `<p>` `200`s, repeating |
| `/hang`                   | accepts and never answers — exercises the attempt deadline |
| `/reset`                  | clears `/flaky` counters and the `/sink` event log |
| `POST /sink`              | webhook receiver: records body+headers, answers `200` |
| `POST /sink?fail=<n>`     | `/sink` but the first `<n>` deliveries get `500` — the retry fixture |
| `POST /sink/hang`         | webhook receiver that never answers — the deliverer deadline fixture |
| `GET /sink/events`        | the recorded deliveries as JSON, newest last |

Why each exists:

- `/hang` and `/slow` prove the deadline bounds the *whole* attempt, headers and body.
- `/flaky` produces the contract's recovery shape: 3 consecutive failures → open,
  2 fresh passes → resolve. `fail=3&pass=2` walks an incident open and shut.
- `/status/301` exists because a probe that follows a redirect is a bug — 3xx is recorded
  as `http_fail`, not chased.
- Loopback targets need `SMON_PROFILE=lab` — the URL-layer SSRF policy rejects private
  literals otherwise.

Rust integration tests stand up their own `axum` stubs; this server is for live proof and
manual exercises (`SMON_WEBHOOK_URL=http://127.0.0.1:58700/...` works for the deliverer too).
