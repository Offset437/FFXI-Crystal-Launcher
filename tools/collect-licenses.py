"""Collect dependency license texts for a Windows release, using Cargo's locked graph."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def dependencies(manifest, target, offline):
    command = ['cargo', 'metadata', '--locked', '--format-version', '1',
               '--manifest-path', str(manifest), '--filter-platform', target]
    if offline:
        command.append('--offline')
    graph = json.loads(subprocess.check_output(command, cwd=manifest.parent))
    nodes = {node['id']: node for node in graph['resolve']['nodes']}
    root_id = graph['resolve']['root']
    pending = [root_id]
    selected = set()
    while pending:
        package = pending.pop()
        if package in selected:
            continue
        selected.add(package)
        for dependency in nodes[package]['deps']:
            if any(kind['kind'] != 'dev' for kind in dependency['dep_kinds']):
                pending.append(dependency['pkg'])
    selected.remove(root_id)
    return [package for package in graph['packages'] if package['id'] in selected]


def license_files(package):
    directory = Path(package['manifest_path']).parent
    root = Path(__file__).resolve().parent.parent
    candidates = set()
    if package.get('license_file'):
        candidates.add(directory / package['license_file'])
    for path in directory.iterdir():
        name = path.name.upper()
        if name.startswith(('LICENSE', 'LICENCE', 'COPYING', 'COPYRIGHT', 'NOTICE')):
            if path.is_file():
                candidates.add(path)
            elif path.is_dir():
                candidates.update(file for file in path.rglob('*') if file.is_file())
    revision_file = directory / '.cargo_vcs_info.json'
    revision = json.loads(revision_file.read_text())['git']['sha1'] if revision_file.exists() else None
    fallback = root / 'licenses/cargo' / (revision or f"{package['name']}-{package['version']}")
    if not candidates and fallback.is_dir():
        candidates.update(file for file in fallback.iterdir() if file.is_file())
    if not candidates and (package.get('source') or '').startswith('git+https://github.com/bpellin/accesskit.git?'):
        # AccessKit's Git workspace keeps the licenses for its member crates at the root.
        for parent in directory.parents:
            apache = parent / 'LICENSE-APACHE'
            mit = parent / 'LICENSE-MIT'
            if apache.is_file() and mit.is_file():
                candidates.add(apache)
                if package.get('license') == 'MIT OR Apache-2.0':
                    candidates.add(mit)
                    chromium = parent / 'LICENSE.chromium'
                    if chromium.is_file():
                        candidates.add(chromium)
                break
    return sorted(candidates)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--offline', action='store_true')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    packages = {}
    for manifest, target in [(root / 'Cargo.toml', 'x86_64-pc-windows-msvc'),
                             (root / 'crystal-loader/Cargo.toml', 'i686-pc-windows-msvc')]:
        for package in dependencies(manifest, target, args.offline):
            packages[package['id']] = package
    texts = {}
    entries = []
    missing = []
    for package in sorted(packages.values(), key=lambda p: (p['name'], p['version'])):
        references = []
        for file in license_files(package):
            content = file.read_text(encoding='utf-8-sig').replace('\r\n', '\n').strip()
            if not content:
                continue
            digest = hashlib.sha256(content.encode('utf-8')).hexdigest()
            if digest not in texts:
                texts[digest] = (len(texts) + 1, content)
            references.append(str(texts[digest][0]))
        label = f"{package['name']} {package['version']}"
        license = package.get('license') or 'see license file'
        location = package.get('repository') or package.get('homepage') or ''
        entries.append(f"{label} | {license} | texts {', '.join(references) or 'not included by upstream'}\n{location}")
        if not references:
            missing.append(label)
    output = ['Crystal Launcher dependency licenses',
              f'{len(packages)} third-party package/version entries; {len(texts)} distinct license and notice texts.',
              'Collected from the locked Windows dependency graphs; individual terms remain applicable.',
              'Build dependencies are included. See THIRD_PARTY.md for non-Cargo code and artwork.',
              'Identical texts are shared by reference. Upstream alternative licenses and copyright notices are retained.',
              '', '\n\n'.join(entries)]
    for index, content in texts.values():
        output.extend(['', f'===== License text {index} =====', '', content])
    args.output.resolve().parent.mkdir(parents=True, exist_ok=True)
    args.output.resolve().write_text('\n'.join(output) + '\n', encoding='utf-8')
    print(f'Collected notices for {len(packages)} packages ({len(texts)} distinct texts).')
    if missing:
        print('Upstream packages without a bundled license text: ' + ', '.join(missing))


if __name__ == '__main__':
    main()
