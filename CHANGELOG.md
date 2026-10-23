# Changelog

## 0.1.0 (unreleased)

First release; not yet published.

- Mail: `client.mail.send(SendMailOptions { .. })` posts to `/mail/send`. `reply_to: Option<String>` is sent as `reply_to`, only when set. `MailSendResult.message_id` is filled from `mail_id` when an older server returns only that.
- Mail: a failed send is `PGBHubError::Api` with `status()` and `code()`; with no mail provider configured that is 503 and `MAIL_PROVIDER_UNAVAILABLE` (exported as a constant). `err.is_mail_provider_unavailable()` checks for it. Sends are never retried.

**Breaking (small)**
- `PGBHubError::Api` gained `code` and `retry_after`; patterns need `..` (or the new `err.status()` / `err.code()`).
- `PGBHubConfig` gained `timeout_secs`, `max_retries`, `retry_delay_ms`; build it with `..PGBHubConfig::default()`.
- `OtpRequestResult.otp` and `MagicLinkRequestResult.token` are removed: the platform never returns them, they are only emailed.

**Fixes**
- `project_id` alone now targets `https://<project>.pgbhub.app`. An empty `endpoint` resolves from the environment, then the project address, then the shared cloud (the default used to be fixed).
- The database name is only sent when configured (`PGBHUB_DATABASE` or `database`); project keys cannot use the default `postgres`.
- `get_schema`, `invoke_action` and storage downloads report the server's message like every other call.
- Requests time out after 30s (there was no timeout).
- `get_user` returns `None` only for a signed-out or expired session; other failures are errors.

**New**
- Reads retry transient failures (502/503/504, connection errors); writes never do.
- Auth: `get_settings`, `sign_up_with_redirect`, `send_magic_link_to`, `resend_verification`, `forgot_password`, `reset_password`, `change_password`, `update_user`, `delete_account`, `get_events`, `list_sessions`, `revoke_session`, `revoke_other_sessions`, `enroll_mfa`, `confirm_mfa`, `disable_mfa`, `verify_mfa`, `oauth_start`, `oauth_callback`.
- `client.actions`: `run`, `list`, `get`, `register` (with effects), `delete`, `deliveries`, `retry_delivery`; `effects_queued` / `effects_preview` on responses.
