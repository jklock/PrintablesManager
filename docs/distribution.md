# Distribution

How to get Printables Manager onto another Mac, with and without notarization.

## What the bundle already has

`scripts/install.sh` always re-signs the installed app with the strongest identity
available in the login keychain, and refuses to finish if the result is ad-hoc:

| Check | Command | Expected |
|---|---|---|
| Identity | `codesign -dvv "/Applications/Printables Manager.app"` | `Authority=Developer ID Application: …` |
| Integrity | `codesign --verify --deep --strict "…"` | silent (exit 0) |
| Gatekeeper | `spctl -a -vvv "…"` | `rejected … source=Unnotarized Developer ID` until notarized |

The installed app carries **no `com.apple.quarantine` flag** (`xattr -l` shows
only `com.apple.macl` and `com.apple.provenance`), which is why it launches on the
Mac that built it even though Gatekeeper reports it as unnotarized. Gatekeeper
only judges a file that arrived with a quarantine flag — i.e. a download.

## Notarizing (needs the account holder)

Notarization requires a credential only the account owner can create. Apple
rejects the account password for command-line tools when 2FA is on, so
`notarytool` needs an **app-specific password** (or an App Store Connect `.p8`
key, which requires approved API access):

```sh
# 1. account.apple.com -> Sign-In and Security -> App-Specific Passwords -> "+"
#    Names: notarytool. Gives xxxx-xxxx-xxxx-xxxx. NOT the iCloud password.
xcrun notarytool store-credentials pm-notary \
  --apple-id "<apple id>" --team-id "<10-char team id>"
# paste the app-specific password at the prompt, never on the command line

# 2. Notarize and staple
NOTARY_PROFILE=pm-notary ./scripts/install.sh

# 3. Verify
spctl -a -vvv "/Applications/Printables Manager.app"   # expect: accepted
xcrun stapler validate "/Applications/Printables Manager.app"
```

If `store-credentials` answers `HTTP status code: 401`, the value pasted was not
an app-specific password. Nothing else produces that 401 — the Apple ID and team
id being wrong give different errors, and the account/team can be confirmed in
App Store Connect's account menu.

## Shipping without notarization

Valid, and what happens today. A Developer ID signature alone still tells the
recipient who signed it; Gatekeeper asks once. The recipient clears the flag:

```sh
xattr -dr com.apple.quarantine "/Applications/Printables Manager.app"
```

Or via Finder: right-click the app → **Open** → **Open** (the button that appears
only after the first refusal). Neither step is needed for an app installed from a
dmg built and copied on the same Mac.

## Do not

- Do not distribute an ad-hoc signature: every rebuild produces a new CDHash, so
  macOS treats each build as a different app and re-prompts for TCC and Keychain
  consent forever.
- Do not put the Apple ID password in a script, a shell history, or a commit.
