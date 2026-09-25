# Releases

A release of the Windows app is made by
[`.github/workflows/release.yml`](../.github/workflows/release.yml) from a
version tag. It leaves a **draft** release; nobody gets it until the owner
publishes the draft.

| File | What it is |
|---|---|
| `oschess-bridge-setup.exe` | The per-user NSIS installer: no administrator, `%LOCALAPPDATA%\oschess bridge`, Ukrainian or English. |
| `oschess-bridge.exe` | The same app without the installer. |
| `SHA256SUMS.txt` | The SHA-256 of both; the release notes list them too. |
| `latest.json`, `oschess-bridge-setup.exe.sig` | What installed apps read to update themselves; made only while the updater secret is set (see [The updater key](#the-updater-key)). |

The workflow runs on GitHub's hosted Windows runners, the one exception to the
own-runners rule in [CLAUDE.md](../CLAUDE.md), because SignPath requires every
job before a signing request to run there. Only a version tag starts it. Its
jobs, in order:

1. **build**: checks that the tag is on `main`, that it names the version in
   `crates/app/Cargo.toml`, and that the updater secret and the public key in
   `crates/app/tauri.conf.json` agree. It then builds `oschess-bridge.exe`
   with the Tauri CLI named in the workflow (`TAURI_CLI_VERSION`, the CLI of the
   `tauri` crates in `Cargo.lock`).
2. **sign-exe**: SignPath signs the executable, once signing is on.
3. **bundle**: builds the installer around that executable.
4. **sign-installer**: SignPath signs the installer, once signing is on.
5. **release**: checks that both files are signed or neither and that the
   signatures are valid, signs the installer for the updater and writes
   `latest.json` while the updater secret is set, computes the SHA-256, and
   creates the draft with the files, the checksums and notes generated from
   the merged pull requests.

The executable is signed before the installer is built around it: Smart App
Control blocks the installed program if only its installer is signed, and an
installer cannot be signed from the inside. The updater's signature is made
last, over the installer as users download it.

## Cutting a release

1. Raise `version` in `crates/app/Cargo.toml`, and in `Cargo.lock` with it, in
   a pull request that is reviewed and merged like any other. That version is
   the installer's, the one the updater compares, and the one the tag must
   name.
2. Tag the merged commit on `main` and push the tag:

   ```
   git fetch origin main
   git tag -a v0.2.0 -m "oschess bridge 0.2.0" origin/main
   git push origin v0.2.0
   ```

3. Follow the run under Actions → release. With signing on, approve both
   signing requests in SignPath; each job waits an hour for its approval.
4. Check the draft before publishing it:
   - It has both `.exe` files and `SHA256SUMS.txt`, and while the updater
     secret is set also `latest.json` naming the tag's version and
     `oschess-bridge-setup.exe.sig`.
   - The downloaded files match their SHA-256
     (`Get-FileHash .\oschess-bridge-setup.exe`).
   - When signed: both files' Properties → Digital Signatures show the
     SignPath Foundation certificate.
   - On a fresh Windows user account, every step of the README's
     [Install](../README.md#install) section happens as written, labels
     included.
   - The manual acceptance of #23: with fresh Chrome and Edge profiles and
     `web = "https://staging.oschess.org"` in `bridge.toml`, allowing the
     local-network prompt connects; denying it shows the permission state with
     the steps to allow it again, not «bridge not installed»; revoking it after
     pairing shows the same, and allowing it again recovers without pairing
     again. «Start with Windows» survives a restart, and a second start opens
     oschess instead of a second bridge.
5. Publish the draft as the latest release. From then on, installed apps with
   updates on find it within six hours.

When something is wrong, delete the draft and the tag
(`git push origin :refs/tags/v0.2.0`), fix it in a pull request, and tag again.
Never move or reuse the tag of a published release: checksums, installed apps
and links refer to it.

## Switching on signing

SignPath Foundation signs open-source projects for free
([conditions](https://signpath.org/terms.html)). Before applying:

- the licence is OSI-approved: MIT;
- every account with write access to this repository, the bots included, has
  two-factor authentication;
- a first release is published, unsigned;
- the README has the [Code signing policy](../README.md#code-signing-policy)
  section, naming the roles and crediting SignPath Foundation;
- the owner applies on [signpath.org](https://signpath.org) and is the
  project's author, reviewer and approver, with multi-factor authentication.

Once SignPath has set up the project, it needs:

- **In SignPath:** GitHub as the project's trusted build system for this
  repository; a signing policy with the owner as its approver; an API token of
  a user who may submit to it; and an artifact configuration for a zip that
  holds one executable, as each request sends first `oschess-bridge.exe` and
  then `oschess-bridge-setup.exe`:

  ```xml
  <?xml version="1.0" encoding="utf-8"?>
  <artifact-configuration xmlns="http://signpath.io/artifact-configuration/v1">
    <zip-file>
      <pe-file path="*.exe">
        <authenticode-sign/>
      </pe-file>
    </zip-file>
  </artifact-configuration>
  ```

- **In GitHub,** Settings → Secrets and variables → Actions:
  - the secret `SIGNPATH_API_TOKEN`;
  - the variables `SIGNPATH_ORGANIZATION_ID`, `SIGNPATH_PROJECT_SLUG` and
    `SIGNPATH_SIGNING_POLICY_SLUG`, and `SIGNPATH_ARTIFACT_CONFIGURATION_SLUG`
    unless the project's default configuration is the one above;
  - last, the variable `SIGNPATH_ENABLED` set to `true`. It is the switch: any
    other value, or none, leaves releases unsigned.

The first signed release needs one more check before it is published. The
installer writes its uninstaller and unpacks the NSIS plugin libraries it
uses at install time, and this workflow signs neither. Install, run and
uninstall the signed release on Windows 11 with Smart App Control on; if Smart
App Control blocks one of those, signing them belongs in the bundle job, in a
pull request of its own.

## The updater key

The updater installs only an installer signed with the updater key, a key of
its own, separate from the code signing certificate. It has been in place since
#51:

- Its public half is `plugins.updater.pubkey` in
  `crates/app/tauri.conf.json`. `cargo test -p app` fails if that value is not
  a public key.
- Its private half and password are the secrets `TAURI_SIGNING_PRIVATE_KEY`
  and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. The build job fails when the
  secret is set but `tauri.conf.json` holds the `PLACEHOLDER` text again, and
  warns in the opposite case.
- The owner keeps the private key and its password where they cannot be lost.
  Installed apps take updates only signed with this key: without it they can
  never be updated again, and everyone has to install the bridge by hand.

To change the key:

1. On your own computer, never in CI or in this repository, generate a new
   one:

   ```
   npx @tauri-apps/cli signer generate -w oschess-bridge-updater.key
   ```

   It asks for a password and writes the private key to
   `oschess-bridge-updater.key` and the public key to
   `oschess-bridge-updater.key.pub`.
2. In a pull request, put the new public key file's content in
   `plugins.updater.pubkey`, and publish a release of it, still signed with
   the old key: installed apps take it, and with it the new public key.
3. Only then replace the secrets with the new private key and its password.

A build from before #51 carries the placeholder and never looks for updates,
so its users install the next version by hand.

The app reads
`https://github.com/asavis/oschess-cb-bridge/releases/latest/download/latest.json`
a minute after it starts and every six hours while «Update automatically» is
on, and whenever the user asks from the menu or the settings. It downloads a
newer version's installer and checks its signature and the version signed into
it (`requireSignedVersion`). Once the bridge is idle, it runs the installer
without a window: no database is downloading or opening, no position index is
being built, no Stockfish is being installed, and no analysis began less than
five minutes ago. The installer replaces the app and starts it again, and the
new start shows «Міст оновлено до X». A draft is never the latest release, so
nothing updates before the owner publishes.
