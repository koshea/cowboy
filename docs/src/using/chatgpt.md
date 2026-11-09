# ChatGPT sign-in

Cowboy can run OpenAI models on your **ChatGPT subscription** instead of an API key,
using OpenAI's [Sign in with ChatGPT](https://developers.openai.com/siwc/token-sharing-open-source)
flow for open-source apps. The provider sits beside any others — a local gateway for
open models, say — so `/model` and the [crew](crew.md) can mix them.

```sh
cowboy providers add chatgpt --chatgpt     # writes the provider, signs in, then offers
                                           # to import your models and add an `openai` crew
cowboy models import chatgpt               # later: your account's models
cowboy crew new openai --preset chatgpt    # later: the built-in all-OpenAI crew
```

The import reads each model's context window and reasoning levels from your
account's catalogue. The preset crew is described in
[How-to](../how-to.md#the-built-in-chatgpt-crew).

Then use it like any model: `/model chatgpt/gpt-6.1-sol` in a session, `cowboy models
use -g chatgpt/gpt-6.1-sol` as the default, a crew slot (`deep: chatgpt/gpt-6.1-sol`),
or a crew's `foreman:` — so switching to, say, an all-OpenAI crew with `/crew openai`
runs the session on your subscription. See [Several crews](crew.md#several-crews).

If you are upgrading from an earlier Cowboy, run `cowboy models migrate` first: the
current build refuses the old bare-name `models.yaml` and single-roster `crew.yaml`.

`--chatgpt` creates a provider like this in `providers.yaml`:

```yaml
providers:
  chatgpt:
    base_url: https://api.openai.com/v1
    api: responses
    auth: chatgpt
```

## Signing in

`cowboy providers login <name>` opens your browser at OpenAI's sign-in page
("Continue with ChatGPT") and waits for it to come back to
`http://127.0.0.1:1455/auth/callback`. If 1455 is taken it uses 1457, then any free
port. The first sign-in registers this installation as **Cowboy** in your ChatGPT
account and asks you to allow it to use your plan. Later sign-ins reuse that
registration and skip the consent screen.

Cowboy checks the result before saving anything:

- the callback must belong to this attempt (`state`);
- the ID token must verify against OpenAI's published signing keys, be for this
  registration, unexpired, and match this attempt's nonce;
- signing in again must be the same ChatGPT account.

If you decline the permission to use your plan, the sign-in is kept but the provider
cannot run models; `cowboy providers list` says so. Run `cowboy providers login`
again and allow it.

**On a remote machine** (SSH), the browser must reach the callback port on the
machine running Cowboy. Forward it first:

```sh
ssh -L 1455:127.0.0.1:1455 your-host
```

Then run `cowboy providers login` on the remote host and open the link it prints in
your local browser. There is no device-code alternative in OpenAI's documented flow.

## Tokens

The tokens live in `~/.config/cowboy/auth/<provider>.yaml` (`0600`), host-side only.
See [the boundary](../security/model.md#provider-credentials-are-never-in-the-sandbox).

- **Access tokens last an hour.** Cowboy refreshes them shortly before they expire,
  and once more if a request is refused with a 401.
- **Refresh tokens last 30 days** and are replaced on every refresh, so using the
  provider keeps the sign-in alive. `cowboy doctor` warns when one is close to
  running out unused.
- **Several processes, one sign-in.** Subagents are separate processes. A refresh
  takes a lock on the record and re-reads it first, so only one process refreshes
  and the others use its result. Without that, a second refresh would spend a token
  that had just been replaced and sign you out.
- When a refresh is refused for good (the sign-in expired, or you disconnected
  Cowboy in ChatGPT), the tokens are cleared and requests fail with
  `run cowboy providers login <name>`. A network error or an OpenAI outage keeps
  them.

`cowboy providers logout <name>` revokes the session at OpenAI and deletes the
tokens. It keeps the registration, so the next login is quick. If revocation cannot
be confirmed, the tokens are still deleted and Cowboy tells you; disconnect Cowboy
in ChatGPT → Settings to be sure.

## What plan usage supports

Requests go to the public Responses API, `https://api.openai.com/v1/responses`.
Plan usage is a preview with these limits, which Cowboy applies for you:

- Every request carries the whole conversation and sets `store: false`, so OpenAI
  keeps nothing between turns. A reasoning model's encrypted reasoning is sent back
  with the next request so it keeps its plan across tool calls.
- Sampling and output settings are not accepted: a model's `temperature`, `top_p`
  and `max_tokens` are not sent. `max_tokens` still sets how much of the context
  window Cowboy reserves for the answer. Fields in `extra` that plan usage rejects
  are dropped.
- Cowboy's tools are sent in a `cowboy` namespace, as plan usage requires. Hosted
  tools (file search, code interpreter, image generation) are not available. The
  agent's own tools do not need them.

## Usage limits and errors

Plan usage errors stop the request. OpenAI never silently bills another way.

- **Usage limit reached**: Cowboy does not retry. Check ChatGPT → Settings → Usage.
  The plan limits there (five-hour on Plus, and weekly) are shared with Codex and
  every app that uses your plan, so Codex use counts against Cowboy too. The
  percentage next to Cowboy under *App limits* is a **cap**, not usage left: 100%
  means Cowboy may use all of what remains.
- **Credits are opt-in.** Once the plan limit is used up, an app draws on your
  ChatGPT credits only if you have allowed apps to use credits in Settings → Usage
  (that switch is separate from buying credits or automatic reload, and Cowboy's app
  limit must be 100%). Codex itself uses credits without that switch, which is why a
  tool signed in through Codex's own login can keep working when Cowboy can't.
- In either case your other providers keep working, so `/model` to one of them, or
  switch to a crew that doesn't use ChatGPT.
- **Temporarily unavailable**: Cowboy retries with backoff and keeps your sign-in.
- **Not eligible, unsupported request, or wrong route**: reported with OpenAI's
  error code and request id, and not retried.

## Caveats

ChatGPT plan usage for open-source apps is a preview run by OpenAI. Its limits, its
models and the flow itself can change without a Cowboy release. If the browser flow
or a request starts failing in a way this page doesn't describe, report it with the
error code and request id from the message.
