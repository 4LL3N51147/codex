# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.

## Banked reset expiry in the status line

Add `banked-reset` to your existing `[tui].status_line` list, or select it in
`/statusline`. It stays hidden until an available banked usage reset expires
within the configured warning window (24 hours by default), then shows, for
example, `Banked reset expires in 2h · /usage`.

```toml
[tui]
banked_reset_warning_minutes = 360 # Warn only in the final 6 hours
banked_reset_check_interval_seconds = 300 # Check every 5 minutes (default: 60)
```

Set `banked_reset_warning_minutes = 0` to disable the warning. Periodic checks
use the configured interval, including while hidden, so the warning appears when
a reset enters the window. The interval must be a positive whole number of
seconds. Normal account and UI updates may also refresh the indicator. It
disappears after expiry or redemption. Resets without a known expiry are hidden.
Use `/usage` to redeem a reset. Reset details come from the existing startup
and usage refreshes; the check interval does not poll the account API.
