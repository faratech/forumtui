# TuiLink — XenForo add-on for the wftui terminal client

`WindowsForum/TuiLink` gives a XenForo 2.3 forum two things for its terminal
users (the [wftui](https://github.com/faratech/wftui) client):

- **A sign-in link that works from any device.** The client shows
  `https://<forum>/tui-start/<id>`; the person opens it in any browser — a
  phone, a laptop on the other side of an SSH session — approves the OAuth
  request, and the code comes back to the terminal by itself. Without the
  add-on the client still signs in, through a browser on the same machine
  (loopback) or by pasting the redirect address.
- **Draft sync.** A reply started in the terminal is waiting in the website's
  editor, and vice versa (`/api/wf-tui-drafts`). Stock XenForo's REST API has
  no draft endpoint at all.

It stores no credentials and no tokens. A sign-in record holds the PKCE
challenge and the client's state for ten minutes; the verifier never leaves
the terminal, so a record cannot exchange a code.

## Install

1. Copy `WindowsForum/TuiLink` into `src/addons/` (or upload the release zip
   through the Admin CP) and install it:
   `php cmd.php xf-addon:install WindowsForum/TuiLink`.
2. Create the OAuth client if the forum does not have one yet: **Setup →
   Service providers → OAuth clients → Add**. Client type **public**; redirect
   URIs `https://<forum>/tui-done` and `http://127.0.0.1/callback`; scopes
   `node:read thread:read thread:write user:read conversation:read
   conversation:write alert:read search:read search:write attachment:read
   attachment:write profile_post:read` (+ `media:read` / `resource:read` if
   the Media Gallery / Resource Manager are installed).
3. **Options → TuiLink → OAuth client ID**: paste that client's ID. (Left
   empty, the add-on uses the client titled `WindowsForum TUI`, which is how
   the original install was wired.)
4. Give users the client ID for their `config.json` (`docs/CONFIG.md` in the
   client repository).

Requires XenForo 2.3.0 or later. No cache provider is needed: sign-in records
live in their own table (`xf_wf_tuilink_link`), swept as they expire.

## Routes

| route | who | what |
|---|---|---|
| `POST /api/wf-tuilink/register` `{state, challenge}` | unauthenticated | `{id, url, ttl}` — a new sign-in record and its short link |
| `GET /tui-start/<id>` | browser | 302 to `/oauth2/authorize` with the record's challenge and state |
| `GET /tui-done?code=…&state=…` | browser (the registered redirect URI) | attaches the code to the record (`?error=` marks it denied) |
| `POST /api/wf-tuilink/poll` `{id}` | unauthenticated | `{status: waiting \| authorized \| denied \| expired, code?}` |
| `GET / POST / DELETE /api/wf-tui-drafts` | bearer token | the account's drafts, keyed as XenForo keys them (`thread-<id>`, `forum-<id>`, `conversation-reply-<id>`) |

The draft endpoints mint no new OAuth scope: each draft kind asserts the
scope its content type already implies, so a token that may post to a thread
may draft to it and nothing else.

## Upgrading from 1.0.x

`php cmd.php xf-addon:upgrade WindowsForum/TuiLink` creates the table. The
1.0.x install found its client by title; set the option to the client ID to
stop depending on the title.

## Manual check after install

- `curl -s -X POST https://<forum>/api/wf-tuilink/register -H 'Content-Type: application/json' -d '{"state":"abcdefgh12345678","challenge":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}'`
  answers `{"success":true,"id":…,"url":"https://<forum>/tui-start/…","ttl":600}`.
- Opening that `url` in a browser lands on the forum's OAuth consent page (an
  "is not configured" message means the option / client title is wrong).
- Declining there, then polling with the `id`, answers `status: denied`.
- From the client: `wftui --site <name>`, `Enter`, approve on a phone.
