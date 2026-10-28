#![cfg(unix)]
use haps::{
    discovery::Discovery,
    model::{PackageSpec, SourceInfo, target},
    repository::Repository,
};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, process::Command};

fn executable(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[tokio::test]
async fn catalog_resume_groups_exact_urls_and_checks_all_history_with_explicit_key()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    let source = root.join("source");
    fs::create_dir(&source)?;
    let real_git = String::from_utf8(Command::new("which").arg("git").output()?.stdout)?
        .trim()
        .to_owned();
    let git = |args: &[&str]| {
        let out = Command::new(&real_git)
            .current_dir(&source)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(&["init"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    fs::write(source.join("README"), "historical contents")?;
    git(&["add", "."]);
    git(&["commit", "-m", "historical"]);
    let old = git(&["rev-parse", "HEAD"]);
    let old_blob = git(&["rev-parse", "HEAD:README"]);
    fs::write(source.join("README"), "current contents")?;
    git(&["add", "."]);
    git(&["commit", "-m", "current"]);
    let new = git(&["rev-parse", "HEAD"]);
    let keys = nostr::Keys::generate();
    let foreign = nostr::Keys::generate();
    let key_path = root.join("publisher.key");
    fs::write(&key_path, keys.secret_key().to_secret_hex())?;
    let wrong_key = root.join("wrong.key");
    fs::write(&wrong_key, foreign.secret_key().to_secret_hex())?;
    let home = root.join("home");
    let directory = home.join("catalogs/fixture");
    let repo = Repository::local(directory.join("packages"))?;
    let selected = Discovery::open(&directory)?;
    let payload = root.join("payload");
    fs::create_dir(&payload)?;
    fs::write(payload.join("hello"), "binary")?;
    let own_a = format!("htree://{}/a", keys.public_key().to_hex());
    let own_b = format!("htree://{}/b", keys.public_key().to_hex());
    let other = format!("htree://{}/a", foreign.public_key().to_hex());
    let spec = |name: &str, version: &str, url: &str, rev: &str| PackageSpec {
        name: name.into(),
        version: version.parse().unwrap(),
        target: target().into(),
        description: "fixture".into(),
        commands: BTreeMap::from([("hello".into(), "hello".into())]),
        dependencies: vec![],
        app: None,
        desktop: None,
        source: Some(SourceInfo {
            git: url.into(),
            rev: rev.into(),
        }),
    };
    for (name, version, url, rev) in [
        ("hello", "1.0.0", &own_a, &old),
        ("hello", "2.0.0", &own_a, &new),
        ("hello", "3.0.0", &own_a, &new),
        ("alt", "1.0.0", &own_b, &new),
        ("foreign", "1.0.0", &other, &old),
    ] {
        let release = repo
            .publish(&keys, spec(name, version, url, rev), &payload)
            .await?;
        selected.ingest([release.event]).await?;
    }
    let catalog = directory.join("packages/catalog.json");
    let original_catalog = fs::read(&catalog)?;
    let original_events = selected.events(vec![nostr::Filter::new()]).await?;
    drop(selected);
    let checkpoint = home.join("publication/retained-checkpoint/uploaded.json");
    fs::create_dir_all(checkpoint.parent().unwrap())?;
    fs::write(&checkpoint, b"existing private checkpoint")?;
    let tools = root.join("tools");
    fs::create_dir(&tools)?;
    executable(
        &tools.join("git-remote-htree"),
        r#"#!/usr/bin/env python3
import json,sys
if sys.argv[1:] == ['--publisher-capabilities']:
 print(json.dumps({'schema':1,'helper':'git-remote-htree','version':'0.2.93','capabilities':['complete-blob-dag-v1']}));sys.exit(0)
print('Usage: git-remote-htree',file=sys.stderr);sys.exit(1)
"#,
    )?;
    executable(
        &tools.join("htree"),
        r#"#!/usr/bin/env python3
import json,os,sys
if sys.argv[1:] == ['--version']: print('htree 0.2.156');sys.exit(0)
with open(os.environ['MARKER'],'a') as f:f.write(json.dumps({'kind':'package-upload'})+'\n')
sys.exit(1)
"#,
    )?;
    executable(
        &tools.join("git"),
        r#"#!/usr/bin/env python3
import json,os,pathlib,subprocess,sys,zlib
args=sys.argv[1:]; real=os.environ['REAL_GIT']; mode=os.environ['MODE']
def record(value):
 with open(os.environ['MARKER'],'a') as f:f.write(json.dumps(value)+'\n')
if 'push' in args:
 i=args.index('push'); url=args[i+2];refs=args[i+3:]
 assert url.startswith('htree://'+os.environ['OWNER']+'/')
 assert all(r==r.split(':')[0]+':refs/tags/haps-source/'+r.split(':')[0] for r in refs)
 record({'kind':'push','url':url,'refs':refs});sys.exit(0)
if 'clone' in args:
 assert '--no-local' in args and '--no-checkout' in args
 assert os.environ['HTREE_PREFER_LOCAL_DAEMON']=='false' and os.environ['HTREE_LOCAL_DAEMON_ONLY']=='false'
 assert os.environ['HTREE_CONFIG_DIR']!=os.environ['PUBLISHER_CONFIG']
 assert not os.path.exists(os.environ['HTREE_DATA_DIR'])
 url=next(a for a in args if a.startswith('htree://'))
 record({'kind':'clone','url':url,'data':os.environ['HTREE_DATA_DIR']})
 args=[os.environ['SOURCE_URL'] if a==url else a for a in args]
 result=subprocess.run([real]+args); result.check_returncode()
 directory=pathlib.Path(args[-1]); odb=directory/'.git/objects'
 if mode in ('MISSING','CORRUPT'):
  for pack in list((odb/'pack').glob('*.pack')):
   data=pack.read_bytes();pack.unlink();pack.with_suffix('.idx').unlink()
   subprocess.run([real,'-C',str(directory),'unpack-objects'],input=data,check=True)
  oid=os.environ['OLD_BLOB']; p=odb/oid[:2]/oid[2:];p.unlink()
  if mode=='CORRUPT':p.write_bytes(zlib.compress(b'blob 5\0wrong'))
 if mode=='SHALLOW':(directory/'.git/shallow').write_text(os.environ['NEW_REV']+'\n')
 sys.exit(0)
if 'rev-parse' in args and '--verify' in args:record({'kind':'revision','rev':args[-1].split('^')[0]})
os.execv(real,[real]+args)
"#,
    )?;
    let mut paths = vec![tools];
    paths.extend(
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .filter(|p| !p.join("git-remote-htree").exists()),
    );
    let path = std::env::join_paths(paths)?;
    let marker = root.join("marker");
    for mode in [
        "NONE",
        "MISSING",
        "CORRUPT",
        "SHALLOW",
        "UNKNOWN_REV",
        "WRONG_KEY",
    ] {
        if mode == "UNKNOWN_REV" {
            let release = repo
                .publish(
                    &keys,
                    spec("hello", "4.0.0", &own_a, &"f".repeat(40)),
                    &payload,
                )
                .await?;
            Discovery::open(&directory)?.ingest([release.event]).await?;
        }
        let before = fs::read(&catalog)?;
        fs::write(&marker, "")?;
        let output = Command::new(env!("CARGO_BIN_EXE_haps"))
            .current_dir(&source)
            .env("PATH", &path)
            .env("REAL_GIT", &real_git)
            .env(
                "SOURCE_URL",
                reqwest::Url::from_directory_path(&source).unwrap().as_str(),
            )
            .env("MODE", mode)
            .env("MARKER", &marker)
            .env("OWNER", keys.public_key().to_hex())
            .env("OLD_BLOB", &old_blob)
            .env("NEW_REV", &new)
            .env("HAPS_NO_DEFAULTS", "true")
            .env("HTREE_CONFIG_DIR", root.join("publisher-config"))
            .env("PUBLISHER_CONFIG", root.join("publisher-config"))
            .arg("--home")
            .arg(&home)
            .args(["catalog", "publish", "fixture", "--key-file"])
            .arg(if mode == "WRONG_KEY" {
                &wrong_key
            } else {
                &key_path
            })
            .output()?;
        assert!(!output.status.success()); // Package uploader intentionally stops before publication.
        let calls = fs::read_to_string(&marker)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            fs::read(&catalog)?,
            before,
            "signed catalog must remain unchanged"
        );
        assert!(
            !home.join("identity.key").exists(),
            "external key must not create another identity"
        );
        if home.join("config.json").exists() {
            let config: serde_json::Value =
                serde_json::from_slice(&fs::read(home.join("config.json"))?)?;
            assert!(
                config["identity"].is_null(),
                "external key must not select another identity"
            );
        }
        assert_eq!(fs::read(&checkpoint)?, b"existing private checkpoint");
        if mode == "NONE" {
            let clones = calls
                .iter()
                .filter(|v| v["kind"] == "clone")
                .collect::<Vec<_>>();
            assert_eq!(
                clones.len(),
                3,
                "one cold clone per exact URL: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                clones
                    .iter()
                    .map(|v| v["data"].as_str().unwrap())
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                3
            );
            let pushes = calls
                .iter()
                .filter(|v| v["kind"] == "push")
                .collect::<Vec<_>>();
            assert_eq!(pushes.len(), 2, "foreign source must never be pushed");
            assert_eq!(
                pushes
                    .iter()
                    .map(|v| v["refs"].as_array().unwrap().len())
                    .sum::<usize>(),
                3
            );
            assert_eq!(calls.iter().filter(|v| v["kind"] == "revision").count(), 4);
            assert_eq!(calls.last().unwrap()["kind"], "package-upload");
            assert_eq!(fs::read(&catalog)?, original_catalog);
            assert_eq!(
                Discovery::open(&directory)?
                    .events(vec![nostr::Filter::new()])
                    .await?,
                original_events
            );
        } else {
            assert!(
                !calls.iter().any(|v| v["kind"] == "package-upload"),
                "{mode} must block publication"
            );
            if mode == "WRONG_KEY" {
                assert!(calls.is_empty());
            }
        }
    }
    Ok(())
}
