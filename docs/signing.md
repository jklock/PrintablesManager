# Signing and notarization

## What the build uses today

`scripts/install.sh` resolves the best identity available on the machine, in this
order, and fails if it finds none:

1. **Developer ID Application** — the only certificate Apple will notarize, and
   the one to distribute with.
2. **Apple Development** — still a *stable* identity across rebuilds, which is
   what stops macOS asking for Keychain and external-volume access after every
   build. It cannot be notarized.

Right now this Mac has only Apple Development certificates
(`Apple Development: jlockenkemper@icloud.com (34WPHG75HQ)`), so builds are
signed with that and notarization is skipped.

## Getting a Developer ID (one-time, done by the account owner)

Either:

- Xcode → Settings → Accounts → select the team → **Manage Certificates…** →
  **+** → *Developer ID Application*, or
- <https://developer.apple.com/account/resources/certificates> → **+** →
  *Developer ID Application*, then double-click the downloaded `.cer`.

After that `./scripts/install.sh` picks it up automatically — no code change.

## Notarizing (credentials never touch the repo or the assistant)

Create a notarytool profile **yourself**, in your own terminal, so the password
is typed by you and stored in your keychain:

```sh
xcrun notarytool store-credentials "pm-notary" \
  --apple-id "you@example.com" \
  --team-id "34WPHG75HQ"
```

It will prompt for an **app-specific password** (create one at
<https://appleid.apple.com> → Sign-In and Security → App-Specific Passwords).

Then build with the profile named:

```sh
NOTARY_PROFILE=pm-notary ./scripts/install.sh
```

The script will submit the dmg, wait for the result, and staple the ticket. If
the identity is still Apple Development it prints why notarization was skipped
rather than failing silently.

## Why the script refuses an ad-hoc signature

Ad-hoc signing derives the signature from the binary's own hash, so every rebuild
is a different app as far as macOS is concerned. That is exactly why the Keychain
and the external library volume kept asking for permission after each update.
