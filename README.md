# Parrot — experimental native Rust port

Native port of Parrot, with behavior updates against .NET reference commit
`4eb01b9acd83cfab7ce8b18491bb311180fff987`. The executable remains `parrot`;
the retained browser frontend, protocols, prompts, skills and Linux C PTY helper
are part of the project. The .NET application is a development reference, not a
native runtime dependency.

**Not a production-ready or release-accepted replacement.** Local session
creation/resume, `serve` and `web` now use the native production service and
security/capability bundle. These paths, existing-owner attachment and explicit
remote `chat --connect` remain experimental; this is not a claim of complete
.NET behavior parity. Do not substitute an unrestricted or test/denying backend.
Templates use MiniJinja; existing Scriban templates require manual conversion.

**Before using existing state:** resuming a selected legacy session can **mutate
its old database and session directory in place**. Back up before any Rust write.
Never run .NET and Rust against the same workspace concurrently. Existing .NET
cannot observe ownership records in the new Rust root, so there is **no global
cross-version locking or simultaneous same-workspace safety guarantee**.

## Operator guide

- [Installation and platform requirements](docs/operator/installation.md)
- [Configuration, legacy selection, backups and rollback](docs/operator/state-and-rollback.md)
- [Third-party notices](THIRD_PARTY_NOTICES.md)

### HTTPS certificates for `serve`

`serve --listen https://127.0.0.1:7443 --token-file <owner-only-file>`
requires an explicitly provisioned server certificate. Use the Kestrel
`Certificates:Default` configuration keys, for example:

```sh
# PEM certificate chain and private key (Password is optional for encrypted keys)
export Kestrel__Certificates__Default__Path=server-chain.pem
export Kestrel__Certificates__Default__KeyPath=server-key.pem
# Or use Path=server.pfx, omit KeyPath, and set Password for a PFX/PKCS12 archive.
```

The same `Kestrel:Certificates:Default:Path`, `KeyPath`, and `Password` keys
can be nested in optional `appsettings.json` and
`appsettings.{Environment}.json` (comments/trailing commas supported).
Precedence, highest first: unprefixed environment variables; environment-specific
JSON; base JSON; `DOTNET_` environment variables; `ASPNETCORE_` environment
variables. Environment variables use `__` in place of `:`. Host environment and
content root are selected before JSON/application environment overrides:
`DOTNET_ENVIRONMENT` overrides `ASPNETCORE_ENVIRONMENT` (default `Production`),
and `DOTNET_CONTENTROOT` overrides `ASPNETCORE_CONTENTROOT` (default current
directory). Relative host content roots resolve against the executable directory;
relative certificate/key paths resolve against the selected content root.

If there is **no `Certificates:Default` section**, setting
`Kestrel__Certificates__Development__Password` (with no Development `Path`)
enables the standard ASP.NET developer-file fallback:
`$APPDATA/ASP.NET/https/{ApplicationName}.pfx`, or, when `APPDATA` is unset,
`$HOME/.aspnet/https/{ApplicationName}.pfx`. `ApplicationName` defaults to `parrot`;
`DOTNET_APPLICATIONNAME` overrides `ASPNETCORE_APPLICATIONNAME` before application
configuration is read. Only a certificate whose entire subject is `CN=localhost`
and which has ASP.NET development OID `1.3.6.1.4.1.311.84.1.1` is accepted through
this fallback. A missing/invalid APPDATA file does not trigger HOME file lookup;
the store fallback below still applies. JSON `null` passwords/key paths are
absent, not empty strings.

For store provisioning, set `Kestrel__Certificates__Default__Subject`, optionally
`Store` (default `My`), `Location` (default `CurrentUser`), and `AllowInvalid`
(default `false`). Path and Subject are mutually exclusive. Selection searches the
full subject case-insensitively, requires a usable private key and serverAuth EKU
(or no EKU), prefers an exact simple-name match, then the latest expiry. With
`AllowInvalid=false`, selection validates the actual certificate chain and time,
not just the leaf dates; revocation is not checked. This option never disables
TLS verification in clients.

Linux reads .NET's passwordless persisted PFX stores beneath
`$HOME/.dotnet/corefx/cryptography/x509stores/{lowercase-store}/`. Trust validation
uses CurrentUser Root/CA stores and system roots (`SSL_CERT_FILE`/`SSL_CERT_DIR`
are honored). Linux LocalMachine stores expose only public read-only Root/CA
certificates, not usable My identities. If neither configured identity nor the
developer-file fallback supplies a certificate, automatic CurrentUser My
selection requires the ASP.NET development OID, current validity, and version 4
or newer, ordered by descending version; the developer certificate itself need
not already be trusted. An empty Default section suppresses only developer-file
lookup, not this later store fallback. A malformed explicitly configured
certificate still fails rather than silently choosing another identity.

macOS supports exportable identities from the default keychain for CurrentUser
My, `/Library/Keychains/System.keychain` for LocalMachine My, and named
CurrentUser keychains under `$HOME/Library/Keychains/{lowercase-store}.keychain`.
Explicit selection uses native trust validation, then exports only the selected
identity. Automatic development selection correlates default-keychain membership
with .NET's `$HOME/.aspnet/dev-certs/https/aspnetcore-localhost-{SHA1}.pfx` files.
**Nonexportable/Secure Enclave keys are not supported**: Parrot requires PEM key
material for its TLS stack, and reports an actionable error instead of silently
choosing a different explicit identity. Native macOS behavior has not been
runtime-verified on the Linux development host.

Certificates must contain a matching private key; PEM/PFX chains are retained.
No certificate is generated or added to any store. Loading occurs at startup,
not through configuration hot reload. HTTPS still requires the owner-only bearer
token file; client certificate/hostname verification is never disabled. HTTP and
Unix-domain-socket serving do not load certificate configuration.
