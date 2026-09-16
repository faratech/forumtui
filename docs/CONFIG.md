# Connecting wftui to a XenForo forum

wftui is a terminal client for XenForo 2.3 forums. With no configuration it
is the Forum Terminal (TUI): it talks to windowsforum.com exactly as it
always has. A `config.json` in its config directory adds other forums, or
overrides fields of the built-in one.

```
Linux:   ~/.config/wftui/config.json
Windows: %APPDATA%\wftui\config.json
any:     $WFTUI_CONFIG_DIR/config.json
```

**Terminal for XenForo** (the edition with no built-in site) asks for these on
its first run: a Setup screen takes the forum's address, the OAuth client ID
and a name, writes them to `config.json` and signs in. Everything below still
applies to it — the file is the same.

`wftui --init-config` writes an annotated example there; `wftui --print-config`
shows the site the client resolved (with the token store it will use, its
user agent and the scopes it asks for); `wftui --help` lists the flags.

## Picking a site

First match wins:

1. `wftui <name>` or `wftui --site <name>`
2. the `WFTUI_SITE` environment variable
3. `default_site` in the file
4. the only site the file lists, if it lists exactly one
5. `windowsforum`

An unknown name is an error that lists the known ones. A file that does not
parse is an error naming the line and column — the client never falls back
to the default site over a broken config.

For any field, the environment beats the file, and the file beats the
compiled-in default: `WFTUI_BASE_URL` replaces `origin` and
`WFTUI_OAUTH_CLIENT_ID` replaces `oauth_client_id` for whichever site was
selected (this is how the test suite and a staging copy of windowsforum.com
are pointed elsewhere).

## The file

```json
{
  "default_site": "windowsforum",
  "sites": [
    { "name": "windowsforum" },
    {
      "name": "example",
      "origin": "https://forum.example.com",
      "oauth_client_id": "PASTE-CLIENT-ID",
      "login": "auto",
      "features": { "xfmg": false, "xfrm": false, "tuilink": null, "drafts_relay": null },
      "brand": {
        "name": "ExampleForum",
        "bold_prefix": "Example",
        "mark": "EX",
        "chrome_bg": "#3A7D44",
        "logo": null
      },
      "quick": [ { "key": "n", "label": "News", "node_id": 12 } ],
      "bot_user_ids": [],
      "prefix_strip": []
    }
  ]
}
```

Every field of a site is optional. An entry named `windowsforum` overrides
only the fields it names; any other name starts from a blank site, so it
needs at least `origin` and `oauth_client_id`. Unknown keys (and `_comment`)
are ignored, so a file written for a newer build loads on an older one.

| field | meaning | default |
|---|---|---|
| `name` | slug: lowercase letters, digits, dashes. Also the directory name under `sites/`. | required |
| `origin` | the forum's origin, `https://host` — no path, query or credentials | required (WF: `https://windowsforum.com`) |
| `oauth_client_id` | the **public** OAuth client the forum's admin created (below) | required (WF: built in) |
| `scopes` | explicit scope list; leave out to derive it from `features` | derived |
| `features.xfmg` | the forum runs the Media Gallery: shows the gallery, its search type, asks for `media:read` | `false` (WF: `true`) |
| `features.xfrm` | the forum runs the Resource Manager: likewise with `resource:read` | `false` (WF: `true`) |
| `features.tuilink` | the forum has the TuiLink add-on (`null` = find out) | `null` (WF: `true`) |
| `features.drafts_relay` | its draft endpoint too (`null` = find out) | `null` (WF: `true`) |
| `login` | `auto`, `tuilink`, `loopback` or `paste` — see below | `auto` |
| `brand.name` | the wordmark in the header and on the sign-in screen | the slug |
| `brand.bold_prefix` | the leading part of `name` drawn bold (`Windows` in `WindowsForum`) | none |
| `brand.mark` | the 1-4 character chip in front of it | first two letters of the slug, upper-cased |
| `brand.chrome_bg` | the header band colour, any CSS colour | WindowsForum blue |
| `brand.logo` | a PNG for the sign-in screen on terminals that draw pixels; relative paths resolve against the config dir | none (WF: built in) |
| `quick` | up to nine `{key, label, node_id}`: a `g <key>` chord, a palette row and a `1`-`9` key on the Home screen each. `key` is one lowercase letter that is not one of `l i a m r d h p g u`. | none (WF: news / security / tutorials) |
| `bot_user_ids` | members whose posts wear an `AI` chip | none (WF: its bot) |
| `prefix_strip` | words dropped from thread prefixes in narrow rows | none (WF: `"Windows "`) |

Each site keeps its own session and drafts: the built-in site in the config
directory itself (where every existing install already has them), any other
site under `sites/<name>/`. A stored session records the origin and client
it was issued for; a store that belongs to another site is set aside as
`token.json.foreign` rather than used.

## What the forum's admin does

XenForo 2.3 ships an OAuth server; nothing needs installing for the basic
client. In the Admin CP, **Setup → Service providers → OAuth clients → Add
OAuth client**:

- **Client type: public.** The client is PKCE-only; it holds no secret.
- **Redirect URIs:** `http://127.0.0.1/callback`. XenForo matches loopback
  redirect URIs regardless of port, so this one entry covers every mode
  the client uses. (`localhost` is *not* treated as loopback — use the IP.)
  If the TuiLink add-on is installed, also add `https://<forum>/tui-done`.
- **Scopes:** at least
  `node:read thread:read thread:write user:read conversation:read
  conversation:write alert:read search:read search:write attachment:read
  attachment:write profile_post:read`, plus `media:read` for the Media
  Gallery and `resource:read` for the Resource Manager if those add-ons
  are installed and the site's `features` say so. Asking for a scope the
  client does not grant fails the whole sign-in.

Save, and hand users the **client ID** (it is public) for their
`oauth_client_id`. That is the whole server side.

### Optional: the TuiLink add-on

The `WindowsForum/TuiLink` add-on (in `addon/` of this repository) adds two
things for a forum's terminal users: a **short sign-in link** that works
from any device — the client shows `https://<forum>/tui-start/<id>`, the
person approves in any browser (a phone, a machine on the other side of an
SSH session), and the code comes back to the client on its own — and a
**draft relay**, so a reply started in the terminal is waiting in the
website's editor and vice versa. Without it, the client signs in through a
loopback redirect (a browser on the same machine) or a paste, and drafts
stay local. Install it, set its option *TuiLink → OAuth client ID* to the
client above, and add `https://<forum>/tui-done` to that client's redirect
URIs. The add-on's README has the details.

## Signing in

`auto` (the default) tries, in order:

1. **Relay** — if the forum answers `/api/wf-tuilink/register`. A 404 means
   the add-on is not there; anything else (a 500, a timeout) is reported.
2. **Loopback** — the client listens on `127.0.0.1` and opens
   `/oauth2/authorize` in a browser. When the browser is on this machine,
   approval comes straight back. When it is not, press `p` and paste the
   address the browser landed on.
3. **Paste** — when no listener could be bound: the browser lands on a
   `http://127.0.0.1/callback?...` address that will not load; paste it (or
   just the `code=` value) into the field.

`m` on the sign-in screen forces a mode for the next attempt and restarts a
running one. The pasted address is checked against the request's `state`;
a bare code is taken as-is (the PKCE verifier still binds it to this
process).

## Troubleshooting

- *"unknown site"* — the name is not in the file; `--print-config` lists
  the known ones in the error.
- *sign-in times out on a forum that has TuiLink* — the OAuth client's
  redirect URIs lack `https://<forum>/tui-done`; press `m` to use loopback
  meanwhile.
- *"provided_redirect_uri_is_not_valid"* in the browser — the client's
  redirect URIs lack `http://127.0.0.1/callback`, or were registered with
  `localhost`.
- *the header colour looks wrong in a 16-colour terminal* — `chrome_bg` is
  quantised to the nearest basic colour there; pick one that survives it,
  or run a 256-colour or truecolor terminal.
- *"token.json.foreign" appeared* — a session for another site or client
  id was in that site's directory (a copied config dir, a changed client
  id); it was set aside untouched and a fresh sign-in was started.
