# Releasing codexy

codexy is this fork's Codex, published to npm as [`codexy`](README.md) by
`.github/workflows/codexy-release.yml`. The workflow builds Codex in release mode for
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl`, assembles each build with `scripts/build_codex_package.py`, and
then packs and publishes the npm packages with `codexy/stage_npm_packages.py`.

## One-time setup

1. Sign up at [npmjs.com](https://www.npmjs.com/signup) and turn on two-factor
   authentication. The first publish of `codexy` makes the account its owner.
2. Enable GitHub Actions for the repository. Workflows inherited from openai/codex run too, on
   pushes to `main` and on pull requests, and fail here for want of their runners and secrets.
   Disable them in the Actions tab if they get in the way.

npm trusts a workflow only for a package that exists, so the first release publishes with a
token:

3. On npmjs.com, under Access Tokens, generate a granular token with read and write access to
   all packages, "Bypass two-factor authentication" checked and a short expiry, such as 7 days.
4. Add it to the GitHub repository as the Actions secret `NPM_TOKEN`, and release (below).
5. Once `codexy` is on npm, open its Settings and add a trusted publisher: GitHub Actions,
   organization or user `huanyuqu`, repository `codex`, workflow `codexy-release.yml`.
6. Delete the `NPM_TOKEN` secret and the token. Later releases publish through the trusted
   publisher, with provenance.

## A release

```sh
git tag codexy-v0.1.0
git push origin codexy-v0.1.0
```

The tag publishes version 0.1.0. A version with a suffix, such as `0.2.0-beta.1`, is published
under the npm tag `next` instead of `latest`, so `npm install -g codexy@next` installs it.

To try a build without publishing, run the workflow from the Actions tab with a version and
`publish` unchecked. Its `codexy-npm` artifact holds the packed packages. npm skips a platform
package it cannot find in the registry, so install the launcher and then unpack your platform's
package where npm would have put it:

```sh
npm install -g ./codexy-0.1.0.tgz
dir="$(npm root -g)/codexy/node_modules/codexy-darwin-arm64"
mkdir -p "$dir" && tar -xzf codexy-0.1.0-darwin-arm64.tgz --strip-components=1 -C "$dir"
```

A version can be published only once. If a publish fails halfway, bump the version.

## Packing locally

```sh
CODEX_REPO_ROOT=$PWD python3 scripts/build_codex_package.py \
  --target aarch64-apple-darwin --cargo-profile release --package-dir /tmp/codex-package
python3 codexy/stage_npm_packages.py --version 0.1.0 \
  --native aarch64-apple-darwin=/tmp/codex-package --out-dir /tmp/codexy-npm
```

`build_codex_package.py` needs Python 3.10 or newer.

## How the package works

The launcher package is `codex-cli/bin/codex.js` with a `package.json` written by
`stage_npm_packages.py`. Its `optionalDependencies` alias each platform's package as
`codexy-<platform>`, which `codex.js` looks up by its own package name. The `codexLaunch` entry
makes `codex.js` run Codex with `--no-daemon` and the context graph turned on, and also applies
to `codexy merge`.

The native build and its `codex-package.json` keep version 0.0.0, as builds from source do, so
Codex does not offer to update to the official release. `codexy --version` prints the codexy
version after Codex's.
