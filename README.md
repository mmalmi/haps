# Haps

**HAshtree Package System** — packages published with your Nostr key, stored in
hashtree, and discovered through your social graph.

Haps is the command-line package manager and shared Rust library. **Hapstore** is
the proposed name for a future graphical app store using that same library.
There is no central account registration or globally owned package-name registry:
a package's identity is its publisher public key plus its name.

Publishing is permissionless. People, organizations, and agents use the same
key-based identity and signed package format. Agents can use qualified package
names and explicit trust policies without an interactive chooser or account signup.

## Status

This is an experimental, working prototype for native programs and regular-file
application directories. It is not yet a replacement for a system package manager,
pip, npm, or Cargo. The manifest and wire formats are not stable.

Implemented:

- Local Nostr identities, signed package cards, immutable release coordinates,
  and signed, publisher-pinned catalogs.
- Content-addressed package files and search indexes using `hashtree-core`,
  `hashtree-fs`, and `hashtree-index`. Every retrieved block is hash-verified.
- Directory and static HTTP(S) sources. Any mirror can serve the signed data.
- Author ranking using the actual `nostr-social-graph` library, imported signed
  follow/mute events, and explicit handling of ambiguous package names.
- Release-specific signed attestations, revocation, and an optional minimum
  number of reviewers from your direct follows. The requirement persists on updates.
- Staged installation, target checks, publisher-preserving updates, previous-version
  rollback, and local removal. Downloads must finish before activation changes.
- Signed [NIP-22](https://github.com/nostr-protocol/nips/blob/master/22.md) comments
  and replies on a package or an exact release. Local mutes hide comments; social
  distance is shown. Comments do not automatically authorize installing software.

The CLI and file layout are designed for macOS, Linux, and Windows. The test suite
compiles and installs a native executable, runs it with arguments, updates it,
rolls back, and exchanges signed comments between separate identities over a local
HTTP source. CI runs that suite on all three operating systems.

## Build and use

Install a prebuilt CLI on macOS (Intel/Apple silicon) or Linux (x86-64/ARM64):

```sh
curl -fsSL https://haps.hashtree.cc/install.sh | sh
```

The installer downloads the published archive from Hashtree, checks SHA-256 and
runs `haps --version` before replacing the executable in `~/.local/bin`. Add that
directory to your `PATH` if needed. It does not use sudo or edit shell startup
files. The checksum verifies the download against the release manifest; it is
served by the same publisher, not a separate trust authority.

To inspect first or pin a version:

```sh
curl -fsSL https://haps.hashtree.cc/install.sh -o install-haps.sh
less install-haps.sh
sh install-haps.sh --version v0.1.3 --bin-dir "$HOME/.local/bin"
```

[Windows x64 zip and all release downloads](https://github.com/mmalmi/haps/releases/latest)
are also available. Extract `haps.exe` into a directory on your `PATH`.
The prebuilt Linux CLI requires glibc 2.35 or newer; application packages have
separate runtime requirements below.

Cargo remains supported on all three operating systems:

```sh
cargo install haps --locked
haps install iris-drive
haps run iris-drive
```

Website: [haps.hashtree.cc](https://haps.hashtree.cc). Source is mirrored on
[GitHub](https://github.com/mmalmi/haps) and
[hashtree](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/haps).

Fresh configurations include the Iris catalog and **Sirius Business Ltd**, the
project-maintenance identity, as a local social-graph starting point. The first
Iris Chat, Iris Drive, and Nostr VPN GUI packages target Apple silicon Macs and
x86-64 Linux (current Arch/Omarchy).
Inspect the starting point with `haps starting-point`, replace it with
`haps starting-point PUBLIC_KEY`, or disable it with `haps starting-point --clear`.
It is a local trust preference, not a published follow event. Explicit local mutes
still block a publisher. Use `--no-defaults` on the first invocation to start with
no catalog or starting point. Removing a catalog and removing the starting point
are independent actions.

```sh
cargo build --locked
cargo run --locked -- --help
cargo test --locked
```

To install the development CLI into Cargo's binary directory:

```sh
cargo install --path . --locked
```

State defaults to the operating system's local application-data directory under
`haps`. Set `HAPS_HOME` or pass `--home` to isolate a publisher, reader, or test.
`haps target` prints the exact build target accepted on this machine.

### Linux desktop and Omarchy

Linux GUI packages register a launcher in `$XDG_DATA_HOME/applications` (normally
`~/.local/share/applications`). Omarchy, GNOME, and other desktop launchers can
then discover them. Updates and rollbacks switch the launcher; removal removes
it. Publisher-qualified, installation-specific filenames prevent name collisions.
Reinstalling repairs a missing entry. Haps refuses to overwrite externally edited
entries. Icons and executables stay inside the verified package directory.

For the Iris apps on current Arch/Omarchy, install system libraries first:

```sh
sudo pacman -S --needed libadwaita fuse3 gst-libav gst-plugin-pipewire gst-plugins-base gst-plugins-good xdg-utils zbar curl
haps install iris-chat
haps install iris-drive
haps install nostr-vpn
```

These Linux binaries require a recent distribution (Iris Chat requires glibc
2.43 or newer). Haps does not yet resolve system dependencies or run installer
hooks. Drive and VPN include their companion CLI binaries; system service setup
and privileged VPN operations remain the apps' responsibility.

To add Find, Install, Installed, Update, and Remove to the **Omarchy v4** menu:

```sh
haps omarchy
# Undo the menu integration:
haps omarchy --remove
```

This uses Python 3, Bash, and Omarchy's terminal launcher and `fzf`. It preserves
existing menu entries/comments and saves the original menu file as
`omarchy-menu.jsonc.haps-backup`. All actions use the same Haps home and trust
checks as the CLI. It adds no automatic trust overrides or background updates.
Use Haps 0.1.2 or newer for the Linux catalog and menu integration.

### Package a program

Stage only the files intended for distribution in a separate directory, for
example `stage/bin/hello`. Put `haps.toml` outside that payload:

Packing respects `.gitignore` and `.ignore` files inside the payload, including
nested rules and negation. It skips Git metadata, OS junk, root build/dependency
caches, and local `.env` files. Ignore rules outside the chosen payload and global
Git ignores do not affect the package. Runtime dependencies nested inside an app
are retained unless that payload's own ignore rules exclude them. Source recipes
explicitly map their declared build outputs into a fresh payload before packing.

```toml
name = "hello"
version = "1.0.0"
target = "aarch64-apple-darwin" # replace with `haps target`
description = "A friendly greeting tool"

[commands]
hello = "bin/hello"
```

Linux GUI manifests can add signed launcher metadata:

```toml
[desktop]
name = "Example"
command = "hello" # key from [commands]; no shell command or extra arguments
icon = "share/icons/example.png" # packaged PNG or SVG
```

On Windows the command path would normally end in `.exe`. Names, paths, size
limits, and command references are validated before installation. Use SemVer;
pre-releases require an explicit `--version` selection. Unknown manifest fields
are rejected, including install hooks and dependencies that Haps cannot yet honor.

```sh
haps identity init                         # prints only the public key
haps pack haps.toml --payload stage --out repository > release.json
haps identity show
haps source add mine ./repository --author PUBLIC_KEY
haps search hello
haps info PUBLIC_KEY/hello
haps install PUBLIC_KEY/hello
haps run hello -- "world with spaces"
haps path hello
haps list
```

`pack` signs and writes a shareable directory; it does not upload or publish to
relays. The directory contains `catalog.json` and sharded `blobs/`. It can be served
unchanged by a static web server, and a reader can add its base URL as a source.
Keep the identity key outside the served directory. Local keys are created with
owner-only file permissions on Unix; Windows relies on the enclosing user profile's
ACL. An external `--key-file` can be supplied to `pack`.

Rebuild the staged payload, increment the manifest version, and run `pack` again
to extend the catalog. Reusing the same publisher/name/version/target is rejected.

```sh
haps update hello
haps rollback hello
haps remove hello
```

Installed versions live in separate directories. The installed-state file switches
atomically after verification. Removal deactivates a package; files remain cached
for recovery. Haps does not alter system PATH, `/usr`, applications folders, services,
or file associations. `haps run` executes the selected command directly without a
shell and passes through its arguments and exit status.

Packages may declare a relative macOS `app = "Example.app"` bundle. `haps run`
opens those bundles through Launch Services; `--command` selects a declared
executable directly. Bundle contents remain inside Haps's versioned install slot.

### Names and shared aliases

```sh
haps alias add alice npub1...
haps install alice/editor
haps install npub1.../editor
haps install editor
```

Aliases use the existing public `~/.hashtree/aliases` file and parser from
`hashtree-config`, shared with `git-remote-htree`. `HTREE_CONFIG_DIR` selects another
shared configuration directory. Haps does not copy aliases into its own settings
or read signing keys to resolve public aliases. Aliases do not imply trust.

When a bare name matches multiple publishers, a terminal offers a numbered chooser
ordered by social distance, then approvals of each publisher's newest matching
release. The choices show the underlying public keys. Scripts receive the same
ranked list and must specify a publisher explicitly. Existing installations retain
their publisher on update.

### Build from a Git repository

```sh
cargo install git-remote-htree --locked
haps identity init
haps build htree://npub1.../project --rev FULL_COMMIT_HASH
haps build htree://npub1.../project --rev FULL_COMMIT_HASH --execute --install
```

The first command previews the committed `haps-build.toml` recipe. Building requires
`--execute`; recipes run with your user permissions and are **not sandboxed**.
Haps delegates `htree://` Git transport to `git-remote-htree` and also accepts HTTPS
and local `file://` Git URLs. Full commit hashes are required; moving branches and
tags are not sufficient build pins. Toolchains must already be available. Submodule
and language dependency resolution are not managed by this initial recipe runner.

See [haps-build.toml](haps-build.toml) for a working recipe. Build commands are
argument arrays, followed by an explicit mapping of output files into the package.
`{exe}` expands to the platform executable suffix, and `target = "host"` selects
the current build target. Haps signs local build results with the builder's identity
and records the original Git URL and commit in the signed package metadata.

`haps source add` also accepts `htree://npub/CATALOG_TREE` for a signed **catalog
directory**, accessed through the public hashtree HTTP gateway. This is separate
from Git source builds. Catalog signatures, publisher pins, content hashes, and
rollback checks remain enforced.

### Social discovery and reviews

```sh
haps identity use YOUR_NPUB                # discovery with an existing public identity
haps import signed-follow-events.json      # one Nostr event or an array
haps search editor
```

A public-only identity can search and install but cannot sign. With a local signing
identity, `haps follow PUBLIC_KEY --export follow.json` updates its local follow
list and exports a signed kind-3 event. It is not automatically sent to relays.

By default installation accepts your own releases or publishers you directly
follow, excluding muted publishers. Other publishers require either explicit
`--allow-untrusted` or a positive `--require-attestations N` policy satisfied by
you/your direct follows. A publisher's self-attestation does not count.

```sh
haps attest RELEASE_EVENT_ID --note "Built and tested this release" --out review.json
haps import review.json
haps install PUBLIC_KEY/hello --require-attestations 1
haps attest RELEASE_EVENT_ID --revoke --note "Found a regression" --out revoked.json
```

Signing a review records a person's assertion; Haps does not independently verify
the claimed testing. Social distance is context, not a security guarantee. Unknown
authors are shown distinctly rather than assigned invented reputation scores.

### Package and release comments

```sh
haps comment hello "Does this support Wayland?" --out comment.json
haps comment hello "This build works here" --release RELEASE_EVENT_ID --out release-comment.json
haps comment hello "Yes, it does" --reply-to COMMENT_EVENT_ID --out reply.json
haps import reply.json
haps comments hello
haps comments hello --release RELEASE_EVENT_ID
```

Import a parent comment before replying. Package discussions use the stable package
address across versions and platforms. Release discussions use an exact event ID,
including its target and content manifest. Replies cannot change their root thread.
Events contain plain text and signatures, suitable for reuse by Hapstore or other
Nostr clients. App-store star ratings, abuse reporting, and moderation beyond local
mutes are not implemented yet.

## One system, several installation environments

The intended scope is cross-platform native apps **and** language packages. Shared
identity, distribution, discovery, comments, and verification belong in Haps core;
dependency solving and environment construction need ecosystem-specific adapters.

| Layer | Responsibility |
| --- | --- |
| Identity and trust | Nostr publisher keys, signed releases, `nostr-social-graph`, attestations, comments |
| Distribution | Hashtree files and indexes, mirrors, eventual Blossom/peer transport |
| Native environment | OS/CPU/ABI selection, app bundles, command exposure, dependencies, rollback |
| Python environment | Python version, wheel tags, markers/extras, isolated environments, build isolation |
| Node environment | Package exports, peer/optional dependencies, workspace and dependency-tree layout |
| Rust environment | Crate versions, features, target dependencies, build scripts, reproducible compiler inputs |
| Hapstore | Discovery, screenshots, reviews, install/update controls over the same core |

Do not flatten Python, Node, and Rust semantics into one generic version solver.
Import existing manifests and support gradual migration. Using Haps to distribute a
Python/Node/Rust runtime is different from resolving that runtime's library packages.

## Reference implementations and feature comparison

These are design references, not runtime dependencies or code copied into Haps.

| Reference | Features to learn from | Consequence for Haps |
| --- | --- | --- |
| [Homebrew](https://github.com/Homebrew/brew) | Versioned Cellar, install receipts, binary bottles, declarative Casks, native app integration | Keep immutable install slots; add explicit app/command integration rather than arbitrary install scripts |
| [pacman/libalpm](https://gitlab.archlinux.org/pacman/pacman) | Dependency/conflict handling, build vs runtime dependencies, transaction lifecycle, preserved config files | Resolve and validate a whole environment before activation; system-package replacement needs much more than downloading files |
| [aurweb](https://github.com/archlinux/aurweb) | Community recipes, comments, votes, outdated flags, maintainer requests | Add signed feedback and curation with social context; distinguish recipe authors, software authors, and reviewers |
| [Zapstore](https://github.com/zapstore/zapstore) | Nostr app identity, app discovery, APK hash/certificate verification, NIP-22 comments | Reuse comment conventions and investigate software-event interoperability before stabilizing Haps metadata |
| [zsp](https://github.com/zapstore/zsp) | Software events 32267/30063/3063, existing-release import, Blossom uploads, remote/browser signers | Support existing release pipelines and signer choices; avoid requiring a new signing account |
| [uv](https://github.com/astral-sh/uv) | Python projects/tools, locking, environments, dependency resolution | Reference for the Python adapter and reproducibility semantics |
| [pnpm](https://github.com/pnpm/pnpm) | Shared content store, isolated dependency layouts, workspaces | Deduplicate content without losing Node's dependency-resolution behavior |
| [Cargo](https://github.com/rust-lang/cargo) | Feature resolution, lockfiles, registries, build plans | Preserve Cargo manifest semantics and compiler integration when adding a Rust adapter |
| [Nix](https://github.com/NixOS/nix) | Build derivations, input-addressed and content-addressed outputs, dependency closures, binary caches | Keep build recipe identity distinct from output content identity; eventually lock the complete environment |
| [OSTree](https://github.com/ostreedev/ostree) / [Flatpak](https://docs.flatpak.org/en/latest/under-the-hood.html) | Hashed file/tree objects, incremental transfer, retained deployments, atomic upgrades | Closest reference for delivering native application trees; preserve file metadata and activate complete verified deployments |
| [gx](https://github.com/whyrusleeping/gx) | IPFS package hashes, hash-pinned dependencies, language adapters | Closest historical reference for decentralized universal packages; study compatibility costs before replacing language tooling |
| [Tangram](https://github.com/tangramdotdev/tangram) | Content-addressed objects, toolchain lockfiles, reusable build steps, lazy filesystem access | Reference for future reproducible builds and environment composition without eagerly downloading everything |

### Content-addressed systems: what to adopt

**OSTree and Tangram are the strongest additional implementation references.**
OSTree's [object model](https://ostreedev.github.io/ostree/repo/) hashes file content
and filesystem metadata, with separate directory objects. Flatpak uses that model
for apps and runtimes. Haps can use hashtree for the storage while adopting the
discipline of verified, complete deployments and explicit filesystem semantics.
Tangram's [object model](https://www.tangram.dev/docs/objects) includes files,
directories, symlinks, and commands with lazy access; its [build and environment
model](https://www.tangram.dev/) locks toolchains alongside libraries.

Nix is useful for dependency closures, immutable environments, and reproducibility,
but a hash in a store path does **not** always mean a hash of the resulting bytes.
Its [manual distinguishes input-addressed outputs from content-addressed outputs](https://nix.dev/manual/nix/2.30/store/derivation/outputs/).
[Floating content-addressed derivations remain experimental](https://github.com/NixOS/nix/blob/master/doc/manual/source/store/derivation/outputs/content-address.md).
Guix is another useful functional-package-system reference, particularly for
[derivations and fixed-output downloads](https://guix.gnu.org/manual/en/guix.pdf).

gx is especially relevant to Haps's original idea: a universal package layer over
IPFS. Its checked-out upstream tip dates to 2020, so treat it as a historical
design reference. The IPFS developers' [2018 reassessment](https://github.com/ipfs/kubo/issues/5850)
records friction around transitive updates, keeping manifests synchronized, and
compatibility with Go modules. Haps should preserve familiar ecosystem workflows
while introducing a new identity/distribution layer, with exact release and content
pins in future lockfiles.

Even [npm already uses a content-addressable cache](https://docs.npmjs.com/cli/v11/commands/npm-cache/),
and pnpm is a useful reference for shared content storage. The intended Haps
combination is hashtree distribution **and search**, Nostr publisher identities,
social-graph discovery, and signed release reviews. Content hashes verify bytes;
signatures identify who published them; social context informs the user's choice.
Availability still requires retained copies and replication.

## Protocol and current limits

The prototype uses experimental kind-30078 app-data events with distinct `d` tags:
`haps/catalog/v1`, `haps/package/NAME`, `haps/release/NAME/VERSION/TARGET`, and
`haps/attestation/RELEASE_ID`. Catalogs and releases include versioned JSON schemas;
their referenced content and search indexes are hashtree CIDs. This is **not yet
Zapstore software-event compatibility**. Package comments use NIP-22 kind 1111 with
an `A` root; release comments use an `E` root.

Catalog authors are pinned on source addition. Saved sequence/event checkpoints
reject older catalogs and changes at an already-seen sequence. This cannot prove
global freshness or prevent a first-time reader receiving an old signed snapshot.
Installed publisher identities and release IDs remain pinned. Missing or corrupt
data is an error, not an empty search result or successful installation.

Remaining work, in order:

1. Live Nostr discovery/comment subscriptions and Blossom replication using the
   existing Hashtree transport/resolver libraries; stable software-event interoperability.
2. Native integration: safe preservation of symlinks and executable metadata,
   broader platform signing/quarantine coverage, installer formats, user command launchers,
   runtime dependencies, removal/garbage collection, and transactional environment locks.
3. Hapstore discovery UI, app metadata/screenshots, reviewer context, reports,
   and package/release discussion over this core.
4. Python, Node, and Rust adapters with their own compatibility and resolution tests.

Current payloads support regular files only: symbolic links, special files, extended
attributes, and installer packages are rejected or unsupported. OS integration
is currently limited to Linux desktop launchers and the optional Omarchy menu.
Only bundles whose required files and metadata fit this format can be published;
verify each installed macOS bundle with `codesign --verify --deep --strict`.
Native code is not sandboxed. No build or install hooks
run during installation. `rollback` explicitly restores a retained local version;
it does not re-evaluate current social approval of that older version.

## Development checks

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests use temporary identities, repositories, and homes. They do not touch real
Nostr identities or publish to public relays. HTTP tests use loopback only.

The static website lives in `website/public` and deploys with the adjacent Wrangler
configuration. Publish that directory separately as `haps-site` on hashtree; the
`haps` source tree and `haps-packages` catalog are separate publications.
Package input manifests and upstream archive checksums live in `packages/`.

### Binary releases

`.github/workflows/release.yml` builds native archives on five platforms from an
existing stable version tag. It requires green Linux/macOS/Windows CI for that
exact commit, checks the crate version against the tag, then extracts each archive
and tests publishing, installing, and running its binary before creating a GitHub
release. Binaries are built once and promoted unchanged to every download channel.

After committing a version bump, pushing `master`, and waiting for CI:

```sh
git tag vX.Y.Z
git push github vX.Y.Z
gh workflow run release.yml --repo mmalmi/haps -f tag=vX.Y.Z
# Once the workflow succeeds, mirror its exact assets using the maintainer's htree identity:
python3 scripts/publish-hashtree-release.py --tag vX.Y.Z
```

The mirror command verifies asset sizes/checksums and the tag's source commit,
then uses `htree release publish` to retain previous versions and move `latest`.
The publisher requires `gh`, `htree`, Python 3.11+, and the release identity already
configured locally. Crates.io publication and website deployment remain separate
steps after their checks. Never replace the files behind an existing release tag.

Installer checks can be run without downloading or executing public binaries:

```sh
python3 -m unittest discover -s tests -p '*_test.py'
actionlint .github/workflows/release.yml .github/workflows/ci.yml
```
