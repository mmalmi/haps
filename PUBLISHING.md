# Publish your package

Haps publishes software under your Nostr public key. There is no registry account,
submission queue, or globally reserved package name. This guide uses Haps 0.1.5
or newer and an already-built program called `my-app`.

## 1. Stage the files

Build your program using its normal build tools. Copy only the executable and
runtime files into a separate directory:

```sh
mkdir -p stage/bin
cp path/to/my-app stage/bin/my-app
```

On Windows, use `New-Item -ItemType Directory -Force stage/bin` and
`Copy-Item path/to/my-app.exe stage/bin/my-app.exe`, then pass
`--command bin/my-app.exe` below. Include required DLLs and resources in the
locations your app expects. Haps does not resolve system dependencies yet.

For a macOS app, stage the whole bundle as `stage/MyApp.app` and use
`--app MyApp.app` instead of `--command`. The current package format supports
regular files and directories; bundles requiring symlinks are not supported.
Preserve the app's platform signing when preparing it.

## 2. Create and check the manifest

```sh
haps init --name my-app --command bin/my-app --payload stage
```

This creates `haps.toml` outside the payload. It never overwrites an existing
manifest, creates a signing identity, uploads files, or prompts for input.
It checks that the declared executable (or bundle metadata) exists. Add `--json`
for machine-readable output. Use `--out` for another manifest filename.

Edit the generated metadata before packing:

```toml
name = "my-app"
version = "0.1.0"
target = "aarch64-apple-darwin"
description = "What your app does"

[commands]
my-app = "bin/my-app"
```

The target defaults to `haps target`. Use `--target` when packaging an executable
built for another platform, and `--version` or `--description` to set metadata
without editing the file. Package names use lowercase letters, numbers, hyphens,
or underscores. Versions use SemVer. Unknown manifest fields are rejected.

Linux GUI packages can also add launcher metadata:

```toml
[desktop]
name = "My App"
command = "my-app"
icon = "share/icons/my-app.png"
```

The command names a key in `[commands]`; the icon is a packaged PNG or SVG.
Declared commands run directly, with no shell or install hooks.

## 3. Sign and test locally

Create a signing identity once, then keep using it for future versions:

```sh
haps identity init
haps identity show
haps pack haps.toml --payload stage --out catalog
```

`identity show` prints your **package publisher public key**. Substitute it for
`PACKAGE_KEY` below. If you already have a Haps identity, skip `identity init`.
Keep its secret key backed up and outside the catalog and payload. You can also
give `pack` an external key with `--key-file`.

`pack` creates or extends a local catalog with signed releases, a search index,
and content-addressed files. It checks the complete payload, including ignore
rules and whether declared commands are included. It does not upload anything.

Use that catalog with the same Haps identity to test your own signed release:

```sh
haps source add mine ./catalog --author PACKAGE_KEY
haps info PACKAGE_KEY/my-app
haps install PACKAGE_KEY/my-app --version 0.1.0
haps run my-app
```

Try the app's basic functionality on its target platform. For GUI apps,
`haps run` opens the bundle or launches the declared executable. A successful
package install alone does not prove the app works.

Only add the source once. After repacking, Haps reads its updated catalog.

## 4. Share the catalog

Install the [Hashtree CLI](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree/rust/crates/git-remote-htree/README.md)
if you do not already have it; for example, `cargo install hashtree-cli --locked`.
Haps does not bundle `htree` or `git-remote-htree`.

Choose a new name for this catalog when first publishing:

```sh
htree add catalog --publish my-packages
htree user
```

`htree user` shows the **hosting identity**. Its public key goes in the
`htree://HOST_NPUB/my-packages` address. The hosting identity can differ from the
package publisher; keep the `--author PACKAGE_KEY` pin tied to the Haps catalog
signer. Never share either secret key.

Alternatively, serve the complete `catalog/` directory from a static HTTPS host.
Readers can use its base URL instead of the `htree://` address. Any mirror can
serve the same signed catalog and blobs without changing their identity.

Give readers these commands, replacing both public keys:

```sh
haps source add maker htree://HOST_NPUB/my-packages --author PACKAGE_KEY
haps alias add maker PACKAGE_KEY
haps info maker/my-app
```

Readers choose their own trust policy. If they want to follow your publishing
key, they can create a Haps signing identity once and follow it:

```sh
haps identity init # skip if already configured
haps follow PACKAGE_KEY
haps install maker/my-app
haps run my-app
```

Alternatively, readers can require release attestations from keys they already
trust. See [release attestations](README.md#social-discovery-and-release-attestations).
Adding a source or alias alone does not authorize installing its packages.

Publish the catalog address, its signer public key, and these instructions
where people can find them. Haps currently searches configured catalogs; uploading
a new catalog does not automatically add it to a global directory.

## 5. Publish updates and other platforms

For an update, rebuild, restage, increment `version`, and repeat:

```sh
haps pack haps.toml --payload stage --out catalog
htree add catalog --publish my-packages
```

Keep the same signing key, catalog directory, and Hashtree name. The local catalog
retains previous releases; replacing it with a fresh directory would lose that
history. A publisher/name/version/target coordinate cannot be republished. Use a
new version even for a small binary fix. Readers run `haps update my-app` and can
use `haps rollback my-app` to return to their previous installed version.

Build and test each platform separately. Use a separate manifest with the same
name and version and the correct `target`, then pack it into the same catalog.
Haps selects the reader's platform. Pre-release versions require an explicit
`--version` when installing.

For source-based installation, provide an inspected build recipe and a pinned
Git commit instead. [The source build guide](README.md#build-from-a-git-repository)
covers HTTPS and Hashtree Git URLs.

## Files included in a package

Packing respects `.gitignore` and `.ignore` files inside the payload, including
nested rules and negation. It skips Git metadata, OS junk, root build/dependency
caches, and local `.env` files. Rules outside the payload and global Git ignores
do not affect the package. Runtime dependencies nested inside an app remain
included unless its own ignore rules exclude them. All symlinks and special
files are currently rejected.

Keep the manifest, catalog output, signing keys, and source working tree outside
the staged payload. Source recipes copy only declared build outputs into a fresh
payload before packing.

Installed versions live in Haps's per-user data directory. Haps verifies files
before activation and keeps prior versions for rollback. On Windows it uses the
user's Local AppData directory; on macOS app bundles stay in their versioned slot
and open through Launch Services. Haps does not add commands to PATH or copy apps
into system application directories. Use `haps run`, `haps path`, and `haps list`
to run and inspect installations.
