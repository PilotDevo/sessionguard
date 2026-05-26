# Setting up `HOMEBREW_TAP_TOKEN`

The release workflow (`.github/workflows/release.yml`) contains a
`homebrew` job that, on every release, automatically:

1. Downloads the platform tarballs from the GitHub Release just published
2. Computes SHA256s for each
3. Renders a fresh `Formula/sessionguard.rb`
4. Pushes the formula to **`PilotDevo/homebrew-tap`**

That cross-repo push requires a token because GitHub's default
`GITHUB_TOKEN` is scoped to the current repo (sessionguard), not the tap
repo. Until the operator creates the token, the job's first step
("Preflight — ensure tap token is configured") **fails loud by design**
on every release. The rest of the pipeline (binaries, crates.io,
GitHub Release) is unaffected.

## One-time setup

1. **Create a fine-grained personal access token.**
   <https://github.com/settings/personal-access-tokens/new>
   - **Resource owner**: `PilotDevo`
   - **Repository access**: *Only select repositories* →
     `PilotDevo/homebrew-tap`
   - **Repository permissions**:
     - **Contents**: `Read and write`
     - **Metadata**: `Read-only` *(implicit when Contents is set)*
   - **Expiration**: as long as you're comfortable with; the workflow
     fails loud when it expires, so you'll know to rotate.
   - Generate the token, copy it.

2. **Store it as a secret on the sessionguard repo.**
   <https://github.com/PilotDevo/sessionguard/settings/secrets/actions/new>
   - **Name**: `HOMEBREW_TAP_TOKEN`
   - **Value**: the token from step 1
   - Save.

3. **Verify on next release.** The next time you push a `v*` tag, the
   `homebrew` job should run cleanly and produce a commit on
   `PilotDevo/homebrew-tap` of the form `sessionguard vX.Y.Z`.

## Rotation

Repeat steps 1 and 2 when the PAT expires. No code changes needed.

## Manual fallback (if you ever want to disable the auto-update)

Edit `.github/workflows/release.yml`, replace the `homebrew:` job's top
with `if: false` to skip it entirely, then update the tap formula by
hand. Less ideal but supported.

## Why a separate token instead of the default `GITHUB_TOKEN`?

The default `GITHUB_TOKEN` automatically minted for each workflow run
is scoped to the *current* repository. Cross-repo writes require an
explicit PAT (or a GitHub App installation, which we're not using
here). The PAT-with-narrow-scope approach keeps the blast radius
small: it can only write to `homebrew-tap`, nowhere else.
