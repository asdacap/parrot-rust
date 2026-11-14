# Use Gemini with an API key

Parrot includes a `google` provider for the [Gemini API](https://ai.google.dev/gemini-api/docs/api-key).
It uses Google's OpenAI-compatible endpoint. You do not need to add a provider
to your configuration or run a proxy.

This is Gemini developer API access, not Antigravity account or subscription
access. Obtain a Gemini API key from [Google AI Studio](https://aistudio.google.com/apikey).
Google's model availability, quotas, and billing apply.

## Supply your key

Choose an environment variable or Parrot's credential store. Do not put your
key in committed configuration, shell command arguments, or shared logs.

To supply the key to the current Bash session without including it in shell
history, run the following commands and enter the key at the prompt.

```sh
read -rsp 'Gemini API key: ' GEMINI_API_KEY
printf '\n'
export GEMINI_API_KEY
```

Alternatively, save the key through Parrot's existing credential-store flow.
If you have not already entered the key, run the prompt above first.

```sh
printf '%s\n' "$GEMINI_API_KEY" | parrot auth login google --api-key-stdin
unset GEMINI_API_KEY
```

A nonempty `GEMINI_API_KEY` takes precedence over the stored `google` credential.
Unset the variable when you want Parrot to use the stored credential.

## Select a Gemini model

List the models available through your configured providers.

```sh
parrot models
```

Choose a `google/` model selector from the output. Replace the placeholder below
with that full selector, including its `google/` prefix.

```sh
parrot chat --new-session --model 'google/<model-id>'
```

Parrot uses streaming Chat Completions for Gemini conversations and preserves
Gemini tool-call thought signatures when continuing with tool results. Model
metadata can be supplemented by the existing models.dev catalog. A catalog
entry does not grant your key access to a model.

## Resolve access errors

If Parrot reports that `google` has no API key, supply `GEMINI_API_KEY` or save a
credential with `parrot auth login google --api-key-stdin`.

If Google rejects a key, model, or request, check the key's project, enabled API,
model access, quota, and billing in Google AI Studio. Rerun `parrot models` rather
than relying on a model ID copied from an older example.

This provider does not add Gemini-native features such as grounding, audio,
video, or image generation. Google's [OpenAI compatibility documentation](https://ai.google.dev/gemini-api/docs/openai)
describes the compatible endpoint and its current limitations.
