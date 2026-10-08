"""Local Semgrep scans using user-selected, hash-pinned rules. Never run package code."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time

MAX_OUTPUT = 32 * 1024 * 1024


def digest(data):
    return hashlib.sha256(data).hexdigest()


def private_dir(path):
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    path.chmod(0o700)
    return path


def save(path, data):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, temporary = tempfile.mkstemp(dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as output:
            output.write(data)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def scanner_env(home):
    # No scanner account tokens, user configuration, or repository PATH entries.
    return dict(PATH=os.pathsep.join(p for p in os.get_exec_path() if Path(p).is_absolute()),
                HOME=str(home), LANG='en_US.UTF-8', SEMGREP_SEND_METRICS='off',
                SEMGREP_ENABLE_VERSION_CHECK='0',
                SEMGREP_SETTINGS_FILE=str(home / 'settings.yml'))


def run_scanner(args, directory, timeout):
    """Bound scanner runtime/output, including workers on Unix."""
    output_path, error_path = directory / 'output.json', directory / 'stderr'
    with output_path.open('wb') as output, error_path.open('wb') as error:
        process = subprocess.Popen(args, cwd=directory, env=scanner_env(directory),
                                   stdout=output, stderr=error, start_new_session=os.name == 'posix')
        try:
            deadline = time.monotonic() + timeout
            while process.poll() is None:
                if time.monotonic() > deadline:
                    raise ValueError('scanner timed out; analysis is incomplete')
                if max(output_path.stat().st_size, error_path.stat().st_size) > MAX_OUTPUT:
                    raise ValueError('scanner output exceeded 32 MiB; analysis is incomplete')
                time.sleep(0.1)
        finally:
            if process.poll() is None:
                if os.name == 'posix':
                    os.killpg(process.pid, signal.SIGKILL)
                else:
                    process.kill()
                process.wait()
    if max(output_path.stat().st_size, error_path.stat().st_size) > MAX_OUTPUT:
        raise ValueError('scanner output exceeded 32 MiB; analysis is incomplete')
    return process.returncode, output_path.read_bytes()


def configure(home, args):
    directory = private_dir(home / 'security')
    policy = directory / 'policy.json'
    if args == ['disable']:
        policy.unlink(missing_ok=True)
        print('Automatic source scans disabled. Existing reports were retained.')
    elif not args:
        print(policy.read_text() if policy.exists() else 'Automatic source scans are disabled.')
    else:
        rules, scanner = args
        content = Path(rules).read_bytes()
        if not content or len(content) > MAX_OUTPUT:
            raise ValueError('rules must be a nonempty local file of at most 32 MiB')
        if not Path(scanner).is_absolute():
            scanner = shutil.which(scanner, path=scanner_env(directory)['PATH'])
        if not scanner:
            raise ValueError('Semgrep is missing; install it and retry, or select --scanner PATH')
        scanner = str(Path(scanner).absolute())
        with tempfile.TemporaryDirectory() as temporary:
            code, output = run_scanner([scanner, '--version'], Path(temporary), 30)
        if code or not output.strip():
            raise ValueError('the selected Semgrep executable cannot run')
        rules_hash = digest(content)
        save(directory / 'rules' / f'{rules_hash}.yaml', content)
        save(policy, json.dumps({'schema': 1, 'scanner': scanner, 'rules_sha256': rules_hash}).encode())
        print('Automatic local source scans enabled before executing source builds.\n'
              'Findings and incomplete scans stop the build. Binary installs are unchanged.\n'
              'Reports stay private. Add --attest-scan to build --execute --install to share its passing result.')


def scan(home, root, revision):
    directory = private_dir(home / 'security')
    policy = json.loads((directory / 'policy.json').read_text())
    if policy.get('schema') != 1:
        raise ValueError('unsupported source scan policy')
    rules_hash = policy['rules_sha256']
    if len(rules_hash) != 64 or any(c not in '0123456789abcdef' for c in rules_hash):
        raise ValueError('invalid rules hash')
    rules = directory / 'rules' / f'{rules_hash}.yaml'
    if digest(rules.read_bytes()) != rules_hash:
        raise ValueError('configured rules changed; enable the intended rules again')
    # Only committed regular files go to the scanner. Unfetched submodules and
    # symlinks are incomplete coverage, never a passing scan.
    entries = subprocess.check_output(['git', '-C', str(root), 'ls-files', '--stage', '-z']).split(b'\0')
    paths = []
    for entry in filter(None, entries):
        metadata, raw = entry.split(b'\t', 1)
        if metadata.split()[0] not in (b'100644', b'100755'):
            raise ValueError('source contains symlinks or submodules; complete scanning is not supported yet')
        relative = Path(os.fsdecode(raw))
        if relative.is_absolute() or '..' in relative.parts:
            raise ValueError('invalid source path')
        paths.append(relative)
    if not paths or len(paths) > 100_000:
        raise ValueError('source scan needs 1–100000 tracked regular files')
    with tempfile.TemporaryDirectory(prefix='scan-', dir=private_dir(directory / 'work')) as temporary:
        temporary = Path(temporary)
        source = temporary / 'source'
        source.mkdir()
        size = 0
        for relative in paths:
            original = root / relative
            if original.is_symlink() or not original.resolve().is_relative_to(root.resolve()):
                raise ValueError('source path escapes checkout')
            size += original.stat().st_size
            if size > 1024**3:
                raise ValueError('source exceeds the 1 GiB scan limit')
            destination = source / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            # Package-provided ignore files must not suppress the user's review.
            if relative.name == '.semgrepignore':
                destination.write_text('')
            else:
                shutil.copyfile(original, destination)
        (source / '.semgrepignore').write_text('')
        code, raw = run_scanner([
                policy['scanner'], 'scan', '--oss-only', '--config', str(rules),
                '--json', '--strict', '--metrics=off', '--disable-version-check',
                '--disable-nosem', '--no-git-ignore', '--no-secrets-validation',
                '--jobs', '2', '--timeout', '10', str(source),
            ], temporary, 600)
        report_path = directory / 'reports' / f'{revision}-{time.time_ns()}.json'
        save(report_path, raw)
        try:
            result = json.loads(raw)
        except ValueError:
            raise ValueError(f'scanner returned invalid JSON; private report: {report_path}')
        if code or result.get('errors'):
            raise ValueError(f'scanner failed or reported incomplete analysis; private report: {report_path}')
        if not isinstance(result.get('results'), list):
            raise ValueError('scanner did not report findings')
        if result['results']:
            raise ValueError(f'{len(result["results"])} finding(s); private report: {report_path}')
        scanned = result.get('paths', {}).get('scanned', [])
        scanned_paths = set()
        for name in scanned:
            path = Path(name)
            path = path if path.is_absolute() else temporary / path
            if not path.resolve().is_relative_to(source.resolve()):
                raise ValueError('scanner reported a path outside the source')
            relative = path.resolve().relative_to(source.resolve())
            if relative in paths and relative.name != '.semgrepignore':
                scanned_paths.add(str(relative))
        if not scanned_paths or not result.get('version'):
            raise ValueError('scanner did not analyze any source files or identify its version')
        summary = dict(schema='haps.source-scan.v1', result='no_findings',
                       source_commit=revision, scanner='semgrep', scanner_version=result['version'],
                       rules_sha256=rules_hash, report_sha256=digest(raw),
                       scanned_files=len(scanned_paths), unscanned_files=len(paths)-len(scanned_paths),
                       checked_at=int(time.time()), binary_source_verified=False,
                       scope='Local static analysis with selected rules; no safety guarantee, dependency audit, or binary provenance verification.')
        save(report_path.with_suffix('.summary.json'), json.dumps(summary, indent=2).encode())
        print(json.dumps(summary))


if __name__ == '__main__':
    try:
        if sys.argv[1] == 'configure':
            configure(Path(sys.argv[2]).absolute(), sys.argv[3:])
        elif sys.argv[1] == 'scan':
            scan(Path(sys.argv[2]).absolute(), Path(sys.argv[3]).absolute(), sys.argv[4])
        else:
            raise ValueError('unknown security operation')
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
