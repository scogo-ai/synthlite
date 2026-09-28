# Security Policy

## Supported versions

Security fixes go into the latest release only. Upgrade to the newest version on the [releases page](https://github.com/scogo-ai/synthlite/releases) before reporting, and check whether the problem still reproduces.

| Version        | Supported |
| -------------- | --------- |
| Latest release | Yes       |
| Anything older | No        |

## Reporting a vulnerability

Please do not open a public issue, discussion, or pull request for a security problem.

Report it privately, by either:

- GitHub private vulnerability reporting: [open a report](https://github.com/scogo-ai/synthlite/security/advisories/new) on `scogo-ai/synthlite`, or
- email to [opensource@scogo.ai](mailto:opensource@scogo.ai).

Include the synthlite version (`synthlite --version`), your OS and architecture, the steps or input that trigger the problem, and what an attacker gains. Redact real API keys, tokens, and private prompts; we never need them.

We aim to acknowledge a report within three business days, agree on a fix and disclosure date with you, and credit you in the advisory unless you prefer otherwise.

## What synthlite guarantees about secrets

These are the properties we treat as security bugs if broken:

- **Keys come from the environment only.** The teacher API key is read from `OPENAI_API_KEY`, or from the variable a config file's `[[key]] api_key_env` names. Config files hold the variable name, never the secret, and a value that is not a valid variable name is refused without being echoed. The Hugging Face token is read from `HF_TOKEN` (or `HUGGING_FACE_HUB_TOKEN`).
- **Secrets are never logged or written.** API keys, tokens, URL credentials, and presigned upload URLs are never printed and never written to the output directory. Base URLs are shown as `host[:port]` only. Error records carry a class and HTTP status, never the prompt or reply text.
- **Hugging Face pushes are private-only.** `synthlite push` creates the dataset repo as private and refuses to upload to a repo that is public. There is no default repo.
- **The Hub token stays on the Hub.** The token is sent only to the Hub API origin (`https://huggingface.co`, or `HF_ENDPOINT` when set). Upload URLs on any other origin, such as presigned storage URLs, receive no token.
- **Teacher replies are data.** synthlite never executes anything a teacher model returns; `--detailed` traces are parsed and checked for structure, not run.

## Out of scope

- What a teacher provider does with the prompts you send it. Every prompt goes to the OpenAI-compatible endpoint you configure; choose one you trust with that data.
- The quality or truth of generated rows. Every row is an unverified teacher generation.
- Vulnerabilities in dependencies that synthlite does not reach. Report those upstream; we track advisories with `cargo deny` in CI.
