# Publish your package

Haps publishes software under your Nostr public key. There is no registry account,
submission queue, or globally reserved package name. This guide uses an
already-built program called `my-app` and Haps 0.1.11 or later.

## 1. Stage the files

Build your program using its normal build tools. Copy only the executable and
runtime files into a separate directory:

```sh
mkdir -p stage/bin
cp path/to/my-app stage/bin/my-app
```

On Windows, use `New-Item -ItemType Directory -Force stage/bin` and
`Copy-Item path/to/my-app.exe stage/bin/my-app.exe`, and set the command path in the manifest to
`bin/my-app.exe`. Include required DLLs and resources in the
locations your app expects. Haps does not resolve system dependencies yet.

For a macOS app, stage the whole bundle as `stage/MyApp.app` and use
`app = "MyApp.app"` in the manifest instead of `[commands]`. The current package format supports
regular files and directories; bundles requiring symlinks are not supported.
Preserve the app's platform signing when preparing it.

## 2. Create and check the manifest

Generate `haps.toml` outside the payload, then review its metadata:

```sh
haps init --name my-app --command bin/my-app --payload stage
```

You can also write the manifest by hand:

```toml
name = "my-app"
version = "0.1.0"
target = "aarch64-apple-darwin"
description = "What your app does"

[commands]
my-app = "bin/my-app"
```

Use the target printed by `haps target`, or the platform you built for when
cross-compiling. Package names use lowercase letters, numbers, hyphens, or
underscores. Versions use SemVer. Unknown manifest fields are rejected.

`init` never overwrites an existing manifest, creates a signing identity, uploads
files, or prompts for input. It checks that the executable (or bundle metadata)
exists. The target defaults to this machine; override it with `--target`.
Use `--version`, `--description`, and `--out` to set metadata and the output path,
and add `--json` for machine-readable output. Review the description before packing.

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

Try the staged app's basic functionality inside a VM or container before sharing.
After publishing, test discovery and installation from a separate Haps profile as
shown below. A successful install alone does not prove the app works.

From 0.1.13, installation also requires an explicit
audit of the exact release by default (`--allow-unaudited` bypasses it for one operation). Another reader needs an audit from their social graph;
your publisher self-audit only counts for your own installation. Provide pinned
source and a matching `haps-build.toml` to support Audit and install. See the
[audit workflow](README.md#release-audits) for source review,
rebuild comparison, and approvals with unverified reproducibility stated.

## Managed event catalogs

For routine publishing, Haps can maintain the output directory and selected
Nostr event index itself:

```sh
haps add haps.toml --payload stage
haps catalog show
haps catalog publish
```

The default catalog is `default`; use `--catalog NAME` on `add` for another
collection and `haps catalog publish NAME` to share it. A local publishing
identity is created on the first staged `add` if none is configured. Keep that
identity for subsequent releases. An identity configured with `identity use`
contains only a public key and cannot sign releases.

Adding a staged release does not upload or install it. Publishing uploads its
content, signs package heads that reference exact releases and immutable
Hashtree roots, and publishes an ordinary `hashtree-nostr` event index. Its root
uses the existing Hashtree kind-30064 announcement format, so other Nostr index
readers can query it without Haps-specific metadata. `haps install
publisher/package` can then resolve the package directly through Nostr relays or
Hashtree event indexes, without resolving a mutable directory catalog.

To include an existing publisher's package, use `haps add publisher/package`
without `--payload`. Its original signed events are preserved. Repeat the
command to refresh the selected records. `haps catalog remove publisher/package`
removes the selection from the next catalog publication; it does not revoke the
publisher's release or uninstall anything.
Empty collections are not published.

The directory-based workflow below remains supported. `haps publish` also
produces direct package heads using the uploaded directory's immutable hash.
Existing readers can still use its named directory catalog.

## 4. Share your package

The prebuilt Haps bundle includes the [Hashtree CLI](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree/rust/crates/hashtree-cli/README.md?g=)
and Git helper. `haps publish` uses a compatible `htree` on PATH or its bundled
copy. If you installed only Haps through Cargo, run `cargo install hashtree-cli --locked`
once if you do not already have it.

Choose a publication name once, then keep it for future updates:

```sh
haps publish catalog --name my-packages
```

The hosting identity can differ from the package publisher. Haps discovers the
location from signed announcements and pins it to the original publisher;
readers find your package automatically. Keep both
signing keys private.

`haps publish` also announces the catalog on your configured Nostr relays using
the Haps publisher's key. It must match the catalog signer; use `--key-file` when
you packed with a separate key. The hosting identity remains independent.
Announcements that do not receive a relay acknowledgement stay queued locally;
retry them with `haps sync`.

Readers can find your package by name or publisher:

```sh
haps search my-app
haps info PACKAGE_KEY/my-app
haps install PACKAGE_KEY/my-app
# Optional local shortcut:
haps alias add maker PACKAGE_KEY
```

Readers choose their own trust policy. If they want to follow your publishing
key, they can create a Haps signing identity once and follow it:

```sh
haps identity init # skip if already configured
haps follow PACKAGE_KEY
haps install PACKAGE_KEY/my-app
haps run my-app
```

Alternatively, readers can require release attestations from keys they already
trust. See [release attestations](README.md#social-discovery-and-release-attestations).
Adding a source or alias alone does not authorize installing its packages.

Share `haps install PACKAGE_KEY/my-app`. Discovery uses preset indexes and
signed announcements, with results ranked by the reader's social graph. No
manual catalog setup is required. A relay or index may not have every package;
failed publication acknowledgements can be retried with `haps sync`.

## 5. Publish updates and other platforms

For an update, rebuild, restage, increment `version`, and repeat:

```sh
haps pack haps.toml --payload stage --out catalog
haps publish catalog --name my-packages
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

## Existing release flows: build once, publish through Haps

Iris Git and `htree release publish` share the same release directory:
`releases/<repo>/<tag>/release.json`, `notes.md`, and `assets/`. Keep that as a
compatibility inventory for existing release pages and app updaters. Haps' signed
`haps.release.v1` events and `haps.files.v1` manifests are the authoritative
package format for installation. Hashtree stores those objects and the native
Nostr event index; it does not define a second software-package format. `latest` selects the Hashtree release;
Haps package-head events select signed package versions for each platform.

New Haps publishers should use `haps add` / `haps pack` and `haps catalog publish`
with the native package format described above; they do not need the older
release inventory. `import-release` is the compatibility boundary for established
build and distribution flows. It never changes Haps' package schema to match a
Hashtree app updater.

The repository's `haps-release.json` maps those existing assets to package
names, targets, commands and app/desktop metadata. It pins the publishing key,
source repository and catalog name. Each selected `release.json` asset needs
its exact size and SHA-256. Asset paths are relative to that release directory;
`{tag}` in mappings substitutes only the explicitly requested tag.

```sh
# Requires Haps with the import-release subcommand, Python 3.9+, and htree for upload.
# HAPS_KEY_FILE may select an existing secret-key file; otherwise Haps uses
# its existing identity.key. Never generate a new identity during a release.
haps import-release stage --config haps-release.json --tag v1.2.3 --check
# After the project's existing release gates and canonical publication:
haps import-release stage --config haps-release.json --tag v1.2.3 --publish
```

`--check` validates the signer, stable tag, checksums and payload layout without
signing or uploading; a draft can be checked but cannot be published. Preparation uses tar/zip/Debian payloads without executing
install scripts. Symlinks and special files fail explicitly, matching Haps'
current package format. The adapter retains regular-file bytes and executable
permissions, including signed macOS bundles. Native installers, Android and iOS
remain on their existing distribution channels; map their desktop/CLI archives
only when those can be installed as ordinary files.

Publication keeps original release events on a byte-identical retry, refuses
changed bytes at an existing version, uploads immutable packages and a native
Hashtree Nostr event index, and requires relay acknowledgement. Failed events
remain queued for `haps sync`; a nonzero release result is not a completed
release. Source commits and package publishers stay pinned during updates.

For date tags, `version_scheme: "date-revision"` maps `v2026.10.5.2` to
`2026.10.502`, retaining ordering and compatibility with existing Iris packages.
Other repositories use stable SemVer tags. Haps' own release mirror runs this
adapter on the same five checksum-verified binary bundles, including helpers.
