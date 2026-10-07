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
- Release-specific signed attestations, trusted warnings, revocation, and an optional minimum
  number of attesters from your social graph. The requirement persists on updates.
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
checks all three executables before activation. The bundle includes `haps`, `htree`,
and `git-remote-htree`. It keeps versioned files under `~/.local/bin/.haps` and adds
command links in `~/.local/bin`; separately installed helpers are preserved. Add
that directory to your `PATH` if needed. It does not use sudo or edit shell startup
files. The checksum verifies the download against the release manifest; it is
served by the same publisher, not a separate trust authority.

To inspect first or pin a version:

```sh
curl -fsSL https://haps.hashtree.cc/install.sh -o install-haps.sh
less install-haps.sh
sh install-haps.sh --version v0.1.10 --bin-dir "$HOME/.local/bin"
```

[Windows x64 zip and all release downloads](#downloads) are also available. Extract the entire zip, keeping `haps.exe`, `bundle.json`, and `libexec/` together,
into a directory on your `PATH`. Haps finds its bundled helpers automatically.
The prebuilt Linux CLI requires glibc 2.35 or newer; application packages have
separate runtime requirements below.

Cargo remains supported on all three operating systems:

```sh
cargo install haps hashtree-cli git-remote-htree --locked
haps install iris-drive
haps run iris-drive
```

This installs Haps and the companion tools for publishing and Hashtree Git builds.
For normal package installs, `cargo install haps --locked` alone is enough: the
shared Hashtree client is compiled into Haps. Cargo and prebuilt installs both
reuse an existing daemon and compatible tools on `PATH`. Git itself and build
toolchains remain separate prerequisites for source builds.

Website: [haps.hashtree.cc](https://haps.hashtree.cc).
[Source and documentation](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/haps)
live on Hashtree, with an additional [GitHub mirror](https://github.com/mmalmi/haps).

Fresh configurations include discovery presets and **Sirius Business Ltd**, the
project-maintenance identity, as a local social-graph starting point. The first
Iris Chat, Iris Drive, and Nostr VPN GUI packages target Apple silicon Macs and
x86-64 Linux (current Arch/Omarchy).
Inspect the starting point with `haps starting-point`, replace it with
`haps starting-point PUBLIC_KEY`, or disable it with `haps starting-point --clear`.
It is a local trust preference, not a published follow event. Explicit local mutes
still block a publisher. Use `--no-defaults` on the first invocation to start with
no discovery presets or starting point. Discovery and trust remain separate preferences.

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
Both desktop launchers and `haps run` add the package's `usr/share` and `share`
directories to the app's resource search path, preserving your existing data
directories. This lets relocated apps find their bundled icons and other data.
After upgrading Haps, reinstall an existing package to refresh its launcher.

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

### Updating Haps itself

Use the same installation route you started with:

- **Cargo:** run `cargo install haps --locked` again. Cargo checks the registry
  version and rebuilds when needed.
- **Prebuilt on macOS or Linux:** rerun the installer. It verifies the downloaded
  archive and executable before replacing Haps. If you originally used a custom
  directory, pass the same `--bin-dir` or `HAPS_INSTALL_DIR` again.
- **Windows zip:** close running Haps processes, extract the new release, and
  replace the complete bundle, including `libexec/`, in the directory where you installed it.
- **Source checkout:** update the source and repeat `cargo install --path . --locked`.

```sh
curl -fsSL https://haps.hashtree.cc/install.sh | sh
haps --version
```

`haps update PACKAGE` updates a package managed by Haps, not the running manager.
Installing or updating a package named `haps` in its internal store would not
replace a separately installed executable on your PATH. There is no `self-update`
command yet, and Haps does not currently depend on `hashtree-updater`.
A future `haps self-update` should use that shared library's signed Hashtree
release resolution and install helpers for prebuilt installs, while directing
Cargo-managed installs back to Cargo.

### Downloads

Prebuilt Haps 0.1.10 archives, hosted on Hashtree:

| Platform | Archive |
| --- | --- |
| macOS Apple silicon | [Download](https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhaps/v0.1.10/assets/haps-v0.1.10-aarch64-apple-darwin.tar.gz) |
| macOS Intel | [Download](https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhaps/v0.1.10/assets/haps-v0.1.10-x86_64-apple-darwin.tar.gz) |
| Linux x86-64 | [Download](https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhaps/v0.1.10/assets/haps-v0.1.10-x86_64-unknown-linux-gnu.tar.gz) |
| Linux ARM64 | [Download](https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhaps/v0.1.10/assets/haps-v0.1.10-aarch64-unknown-linux-gnu.tar.gz) |
| Windows x64 | [Download](https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhaps/v0.1.10/assets/haps-v0.1.10-x86_64-pc-windows-msvc.zip) |

The [release manifest](https://upload.iris.to/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/releases%2Fhaps/v0.1.10/release.json) records archive sizes and SHA-256 checksums.

### Publish your package

Start with a built program and stage its runtime files in `stage/`. Generate
`haps.toml`, review its metadata, then sign your catalog:

```sh
haps init --name my-app --command bin/my-app --payload stage
haps identity init # once; keep the same key for future releases
haps pack haps.toml --payload stage --out catalog
```

`pack` signs a local catalog. Test it locally, then publish it
with `haps publish catalog --name my-packages` or serve the directory over HTTPS.

The **[publishing guide](PUBLISHING.md)** covers staging, testing, Hashtree setup,
sharing install commands, multiple platforms, ignore rules, and updates. No
registry account or submission is required. Readers find packages by name or publisher through preset indexes and signed
announcements. No manual catalog setup is required.

`haps init` generates a manifest without prompts and never overwrites an existing
one. The guide also covers writing the manifest by hand.

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

When a bare name matches multiple socially trusted publishers, a terminal offers an arrow-key chooser
ordered by social distance, then trusted attestations of each publisher's newest
matching release. Enter selects; Escape cancels. Each choice shows a local alias
(or full public key), the version, social distance, and attesters. Selection does
not bypass trust policy. Existing installations retain their publisher on update.
The chooser shows up to three connections who follow each publisher and how many
more there are, using the same rule as Iris Contacts. Local aliases label the keys;
JSON output includes every matching public key in `followed_by`. Muted connections
are excluded. Numeric distances remain available in JSON for ranking.

For agents and scripts, `--non-interactive` (or `HAPS_NON_INTERACTIVE=true`) disables
prompts even in a terminal. Redirected stdin, stdout, or stderr also disables the chooser.
`search`, `info`, `install`, `update`, `list`, `attest`, and `warn` accept `--json`, which
implies non-interactive mode. Search and list return one JSON array; info returns
one release object; install/update return `{ "status": "installed", "release": ... }`.
`attest` and `warn` return the signed event. Runtime failures return a JSON `error` object and
a nonzero exit status. Ambiguous names return `code: "ambiguous_package"` and a
socially ranked `candidates` array, including full publisher keys, release IDs,
and signed attestations and warnings. Diagnostics stay on stderr. CLI argument errors use
the normal usage message on stderr.

```sh
haps search hello --json
haps install npub1.../hello --version 1.0.0 --require-attestations 2 --json
haps list --json
```

Choose an explicit publisher after inspecting the candidates; Haps never silently
accepts the first match for an agent. Use public keys for reproducible automation;
local aliases are conveniences, not global identities.

### Build from a Git repository

```sh
# Cargo-only install: cargo install git-remote-htree --locked
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

Discovered package locations can be `htree://` directories, resolved from signed
Hashtree roots and read as verified content. This is separate from Git source builds. Catalog signatures, publisher pins, content hashes, and
rollback checks remain enforced.

### Social discovery and release attestations

```sh
haps identity use YOUR_NPUB                # discovery with an existing public identity
haps import signed-follow-events.json      # one Nostr event or an array
haps search editor
```

A public-only identity can search and install but cannot sign. With a local signing
identity, `haps follow PUBLIC_KEY --export follow.json` updates its local follow
list and exports a signed kind-3 event. It is not automatically sent to relays.

Search and bare-name installation only include releases authored or vouched for by
your social graph. Authors and vouchers must be reachable and not overmuted. Haps
uses `nostr-social-graph`'s nearest-distance mute rule with threshold `3`, matching
Iris feeds and recommendations: at the nearest distance with opinions, hide when
`muters * 3 > followers`. Your own follow takes precedence over more distant
opinions; your direct mute always blocks installation.

An explicit `haps install npub.../package` (or public key/alias plus package)
can install outside that filter, with a warning. It does not add trust or change
future bare-name updates. `--allow-untrusted` remains an explicit override. Neither
form bypasses direct mutes, release warnings, or `--require-attestations N`.
A vouch approves only the exact signed release; a publisher's self-attestation does
not count. A new version needs its own vouches if its publisher is outside the
eligible graph. Haps never silently chooses an older vouched-for version instead.

```sh
haps attest alice/hello --version 1.0.0 --note "Built from source; tests pass" --out attestation.json
haps install PUBLIC_KEY/hello --require-attestations 1
haps warn alice/hello --version 1.0.0 --note "Unexpected outbound connection; report: https://example.org/report" --out warning.json
haps attest RELEASE_EVENT_ID --revoke --note "Withdrawing my earlier endorsement" --out revoked.json
```

The shortcut requires an explicit version and resolves the exact signed release
for the current platform. Use `--target TARGET` for another platform, or pass a
release event ID directly. Without `--out`, the signed event is saved under
`HAPS_HOME/attestations/EVENT_ID.json`. It is also kept locally and queued for relay publication. Failed sends remain
queued until acknowledged; retry with `haps sync`. Readers fetch findings when
searching, inspecting or installing that release.

Install output shows **Attested by** and the signed notes. `info --json` includes
the full signed events and signer keys. Only current positive attestations from
reachable, non-overmuted members of your graph count; publisher self-attestations, muted signers,
revocations, and attestations of a different release do not count. A future version
or another platform needs its own attestations.

#### Warn about a release

`haps warn` is a negative finding, distinct from withdrawing an endorsement. It
accepts the same exact-release selector, `--target`, `--out`, and `--json` options
as `attest`. It requires Haps 0.1.6 or later.

Only **your key and reachable, non-overmuted graph members** contribute endorsements
or warnings. Strangers do not appear as trusted findings or affect installation.
Findings are retrieved from configured Nostr event indexes and relays before
filtering results or choosing a publisher. The configured starting point counts as a direct connection.
A publisher cannot endorse its own release for your approval threshold, but a
followed publisher can warn about its own release, for example to recall a build.

An active trusted warning blocks `install` and `update`, even with
`--allow-untrusted` or enough other endorsements. Inspect the signer, note, and
signed event with `haps info PACKAGE --version VERSION --json`; its `warnings`
array uses the same social filter. A deliberate `--allow-warnings` override applies
only to that operation and never lowers the endorsement requirement.

```sh
haps warn RELEASE_EVENT_ID --revoke --note "Finding withdrawn after investigation"
```

Withdrawing leaves a neutral claim: it does not endorse the release. For each
signer and release, the latest signed claim wins across `attest`, `warn`, and
withdrawals. A new endorsement replaces that signer's earlier warning; an old
replayed event cannot restore it. Other signers' findings are unaffected. Warnings
never silently remove or stop an installed app. `rollback` retains its existing
explicit recovery behavior and does not re-evaluate social policy.

For discussion applying to a package across versions, use `haps comment`.
Endorsements and install-blocking warnings remain tied to exact release hashes.
Haps 0.1.5 reads a warning as a withdrawn endorsement but does not enforce its
warning policy; readers need 0.1.6 for that.

#### Set up an agent to scan releases

Give the agent its own signing identity and the packages it should scan, then run
it using your scheduler or agent service. Other users can follow its public key
and see its signed findings automatically. It gains no special authority: the same social
filter applies to people and agents.

For example, create a persistent agent profile once (POSIX shell):

```sh
export HAPS_HOME="$HOME/.local/share/haps-scan-agent"
haps identity init
haps identity show
haps search my-app
```

Use that same `HAPS_HOME` in the scheduled job. Back up its signing key and share
only its public key and exported events.

A job should select an exact release with `haps info --json`, inspect that release's
files and source in an isolated test environment, and record the checks performed
and evidence. Pin the result to `release_id`, never a moving package name. Sign an
endorsement only after the declared checks complete; sign a warning for a concrete
finding. A timeout, unavailable scanner, or incomplete analysis is not a pass.

The following shell example uses `jq` and **your own** `scan-release` program.
That program takes the release JSON, verifies it is scanning the referenced
payload, writes a JSON `summary` and matching `release_id`, and exits 0 for a pass,
1 for a finding, or another status for an incomplete scan. Haps does not include a
scanner or scheduler.

```sh
set -eu
haps --non-interactive info PUBLISHER/my-app --version 1.0.0 --json > release.json
release=$(jq -er '.release_id' release.json)
result=0
./scan-release release.json > scan.json || result=$?
# Reject results for a different release or without an evidence summary.
jq -e --arg release "$release" '.release_id == $release and (.summary | type == "string" and length > 0)' scan.json >/dev/null
note=$(jq -er '.summary' scan.json)
case "$result" in
  0) haps attest "$release" --note "$note" --out claim.json --json ;;
  1) haps warn "$release" --note "$note" --out claim.json --json ;;
  *) echo "Scan incomplete; no new claim signed" >&2; exit 1 ;;
esac
```

The signing command publishes its event through the same durable outbox as
package announcements. Readers follow the agent and inspect its exact-release
findings with `haps info`; `--require-attestations 1` can enforce the local policy.
Exports remain useful for offline transfer, but are not required for discovery.
Keep prior findings unless new evidence changes them; use `--revoke` to withdraw
your claim. Include scanner versions, checks and evidence in the note.

An attestation is a signed claim by a person or agent about an exact release, such
as tests run or code audited. It is not a product rating or automatic proof of a
security audit. Haps verifies the signature and scope, not the claimed work.
Social distance is context, not a security guarantee.

### Package and release comments

From Haps 0.1.11, comments, endorsements and warnings publish automatically and
readers retrieve them through Nostr. Signed exports and imports remain available
for offline use. Discovery is bounded; a quiet or unavailable relay does not prove
that there are no findings.

```sh
haps comment hello "Does this support Wayland?"
haps comment hello "This build works here" --release RELEASE_EVENT_ID
haps comment hello "Yes, it does" --reply-to COMMENT_EVENT_ID
haps comments hello
haps comments hello --release RELEASE_EVENT_ID
```

Haps retrieves package and release comments before displaying a thread or replying. Package discussions use the stable package
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
social-graph discovery, and signed release attestations. Content hashes verify bytes;
signatures identify who published them; social context informs the user's choice.
Availability still requires retained copies and replication.

## Catalogs and mirrors

Discovery sources are preset or learned from signed announcements. Search uses
those sources automatically and ranks publishers using your social graph. Catalog signatures and package signatures
are checked separately: hosting or indexing a package does not make its host the
package author.

Catalogs, search indexes, and files are content addressed. Copies keep the same
content hashes, so the data can move between servers or be mirrored unchanged.
Keep `catalog.json` and its referenced `blobs/` together. A mirror keeps the
original catalog signer; changing a catalog requires signing a new catalog under
your own key. Use `haps add publisher/package --catalog NAME` to curate original signed
package events in a named catalog.

The reader supports local directories, HTTP(S), mutable `htree://npub/name`
catalogs, and immutable `htree://nhash...` payloads and indexes. Hashtree
reads use `hashtree-client`: an existing local daemon supplies signed root events
through its relay and raw blocks through its shared cache and peer connections.
Without a daemon, the same binary uses the relays and Blossom servers from the
shared Hashtree configuration. No public directory gateway is required.

Haps observes signed roots for a bounded window, then reads one immutable catalog
snapshot. Hosting signatures, catalog signatures, publisher pins, block hashes,
and catalog rollback protection remain separate checks. No daemon is started
and no identity is created just to download a package.

`HTREE_CONFIG_DIR` selects the shared configuration. `HTREE_DAEMON_URL` can select
a loopback HTTP endpoint; otherwise the configured server port is used.
`HTREE_PREFER_LOCAL_DAEMON=0` skips the daemon. `HTREE_LOCAL_DAEMON_ONLY=1` disables
external fallback. `NOSTR_RELAYS` overrides standalone relays (comma-separated).

Haps keeps verified download caches and installed/rollback files in its own data
directory. Daemon cache eviction is safe: Haps retains the bytes it needs and never
opens the daemon's database or alters pins belonging to another application.

## Protocol and current limits

Legacy directory catalogs, package cards, and releases use kind-30078 app-data events with
`d` tags `haps/catalog/v1`, `haps/package/NAME`, and
`haps/release/NAME/VERSION/TARGET`. Their content and search indexes are Hashtree
CIDs. Package comments use NIP-22 kind 1111 with an `A` root; release comments use
an `E` root. This is not yet Zapstore software-event compatibility.
Package heads use kind 32267; event catalogs use the native Hashtree Nostr index
and kind-30064 root events described below.

Release attestations use the shared [fact-event format](https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/nostr-social-graph/nips/fact-events.md?g=):
kind `37368`, empty content, and the exact release event hash in both
`["i", "<hash>", "subject"]` and `["d", "<hash>"]`. Required facts are
`type=haps_release_attestation`, `schema=1`, `approved=true|false`, and a `note`.
An optional `warning=true|false` defaults to false for older events; approval and
a warning cannot both be true.
New snapshots include millisecond metadata; Haps compares timestamp, milliseconds,
then event ID as defined by the shared helpers. `warn` writes `approved=false` and
`warning=true`; `--revoke` writes both false.
Each signer counts at most once per release, regardless of how many copies are
imported. One signer cannot replace another's decision.

Existing kind-30078 `haps/attestation/RELEASE_ID` events remain readable and share
the same signer/release slot with new snapshots. They use their second timestamp
when compared with new snapshots; an exact time tie prefers the fact snapshot.
Legacy-only ties retain the original ordering. Older Haps versions cannot read
the new format. Other fact profiles, identity links, ratings, and disputes do not
authorize installation. Signatures prove authorship, not the claimed testing or
audit work.

Discovery pins package locations to the signed publisher identity. Saved sequence/event checkpoints
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

## Package discovery and event catalogs

A package is identified by its publisher and name. Haps queries signed Nostr
package events by kind **32267**, publisher, and `d` tag. The current package
head names exact release event IDs, versions, platforms, and an immutable
Hashtree payload root. Installation verifies the original release signature and
file hashes; it does not need a mutable catalog root for these package heads.
Existing catalog-location announcements and signed directory catalogs remain
readable for compatibility.

`nostr-pubsub` routes the same event filters to Hashtree-backed Nostr indexes and
relays. Indexes contribute independent results, duplicates are merged, and
addressable events use newest timestamp/lowest-event-ID ordering. Live relay
observations remain open past EOSE for a bounded window. Unavailable sources are
reported; a quiet relay cannot establish that a package does not exist.

Your catalog is an ordinary `hashtree-nostr` event index containing a selected
collection of original signed events, separate from
your browsing cache and installed-package list. Add your own staged package:

```sh
haps add haps.toml --payload stage
haps catalog show
haps catalog publish
```

`add` creates a local Haps publishing identity if none is configured. It signs
the staged release and adds it to the `default` catalog. It does not upload,
install, or execute the package. `catalog publish` uploads content, exports the
selected Nostr event index, and announces its immutable address. Publication
uses the existing `htree` helper, included in the Haps bundle.

You can also curate other publishers' packages without changing their signatures:

```sh
haps add npub1.../editor --catalog favorites
haps catalog show favorites
haps catalog publish favorites
haps catalog remove npub1.../editor --catalog favorites
```

Repeat `add` to refresh a curated package's records. Removing a selection affects
future publications of that catalog; it does not uninstall the package or revoke
the original publisher's release. Publishing shares the selected events, not
private identity keys, browsing history, or installed-package receipts.

Catalog roots use ordinary signed Hashtree kind-30064 events with `l=hashtree`,
`hash`, optional `key`, and `d=nostr-event-index` for general indexes. Haps publishes
curated collections under `nostr-event-index/<name>`, including `/default`, so
they do not replace an existing general archive. No Haps-specific `index.json`
wrapper is required. Haps can query mixed Nostr indexes maintained by existing
Hashtree indexers; it filters for package events itself. Root announcements and
signed follow/mute records are queried through the same `nostr-pubsub` router,
using known indexes and relays. Catalogs from known people up to two follow hops
away are discovered with bounded author queries. Muted and overmuted owners are excluded. The social graph
filters and ranks package publishers; an index owner's signature does not grant installation
trust to the packages they collect.

```sh
haps catalog discover
haps catalog list
haps search editor
# Explicitly include another signed index:
haps catalog add community htree://npub1.../nostr-event-index --author npub1...
```

Search combines configured indexes, socially discovered indexes, the local event
cache, and available relays. `--no-defaults` stays offline unless networking is
explicitly configured. The shared Hashtree daemon is used when available;
`NOSTR_RELAYS` selects standalone relays. Failed publications remain in a durable
outbox until a relay acknowledges them; retry with `haps sync`.

The existing public Haps index remains a discovery preset. Its configured
worker refreshes roughly every 30 minutes and retains up to 2,048 recent
records; it is not a complete directory. The advanced `haps index build --out
package-index` command still exports a broad discovery-cache index for workers.
For personal curation, use `haps add` and `haps catalog publish` instead.
Older directory-based Haps indexes remain readable for compatibility.

For a dedicated Linux worker, `scripts/index-worker.py` refreshes and publishes
only changed indexes, retries failed uploads, refuses empty publications and
stops at a time, storage or free-space limit. The example units in
`integrations/systemd/` use a separate service account, 25% of one CPU, 256 MiB
RAM, a 90-second deadline, a 128 MiB state budget and a 30-minute cadence. They
are templates, not an enabled service. Set up the worker identity, networking
and executable paths before a single manual canary run; enable the timer only
after measuring its effect. A process limit does not bound work delegated to a
shared Hashtree daemon.
If the local daemon does not carry package announcements, set
`HTREE_PREFER_LOCAL_DAEMON=false` for the worker to query its configured relays
directly. Give publication its own `HTREE_CONFIG_DIR`, identity and storage.

## Development checks

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

The regular suite uses temporary identities, repositories and homes, with loopback
relays and HTTP servers. It covers announcement retries, publisher discovery,
signed shared indexes and an install by `npub/package` without a registered catalog.
A three-user relay test covers discovery, comments and replies, socially filtered
endorsements and warnings, installation blocking, and revoked findings without
manual event imports. A separate opt-in `tests/public_discovery_e2e.py` runs the
publisher and reader in different disposable containers against public relays
and Hashtree; its public proof file contains no secret keys or catalog URL. The `review` and
`read-feedback` roles extend that check to three users, checking remote comments,
socially relevant warnings and installation blocking. Run each role in a fresh
container with Haps installed; pass only the previous role’s JSON output on stdin.

CI additionally installs **Iris Chat, Iris Drive and Nostr VPN from the public
catalog** on Apple silicon macOS and x86-64 Linux. This downloads the actual signed
releases into an isolated home, verifies the executable and receipt, checks macOS
code signatures and Linux desktop launchers, then removes the temporary install.
It does not launch the apps or prove that their platform dependencies are present.
Windows has native installer/lifecycle tests; these example packages do not yet
have Windows releases in the catalog.

Run the public-network smoke test **inside a VM, container, or CI runner**, never
on a personal desktop (Iris Chat alone by default):

```sh
HAPS_TEST_ISOLATED=1 cargo test --locked --test published_apps -- --ignored --nocapture
HAPS_TEST_ISOLATED=1 HAPS_TEST_APPS=iris-chat,iris-drive,nostr-vpn cargo test --locked --test published_apps -- --ignored --nocapture
```

Public catalog or transport outages fail this acceptance check; they are not
silently skipped. Ordinary `cargo test` remains independent of public services.

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
