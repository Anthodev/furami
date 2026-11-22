"""Map every actual payload owner and static contributor to original terms."""
import json
from pathlib import Path
import re
import hashlib


UBUNTU_TERMS = Path(__file__).with_name('ubuntu-terms.lock.json')


def verify_ubuntu_terms(ubuntu, owners, elf_paths, source_only_labels):
    terms_bytes = UBUNTU_TERMS.read_bytes()
    terms = json.loads(terms_bytes)
    receipt_identities = {f'{row["binary_package"]}@{row["binary_version"]}' for row in ubuntu['packages']}
    if (receipt_identities != set(terms['packages']) or len(receipt_identities) != len(ubuntu['packages']) or
            ubuntu.get('terms_lock_sha256') != hashlib.sha256(terms_bytes).hexdigest()):
        raise ValueError('Ubuntu component terms lock/set mismatch')

    expected = {}
    for row in ubuntu['packages']:
        package, version = row['binary_package'], row['binary_version']
        pinned = terms['packages'][f'{package}@{version}']
        refs = re.findall(r'LicenseRef-[A-Za-z0-9.-]+', pinned['license_expression'])
        pinned_texts = {**terms['custom_license_texts'], **pinned.get('custom_license_texts', {})}
        if row.get('custom_license_texts') != {name: pinned_texts[name] for name in refs}:
            raise ValueError('Ubuntu component custom license texts changed')
        if (row['source_package'] != pinned['source_package']['name'] or
                row['source_version'] != pinned['source_package']['version'] or
                any(row.get(key) != pinned[key] for key in
                    ('notice', 'license_expression', 'license_exception', 'source_required', 'shipped_objects')) or
                row.get('terms_selection_resolved') is not True):
            raise ValueError(f'Ubuntu component pinned terms correspondence mismatch: {package}={version}')
        expected[f'ubuntu:{package}={version}'] = set(pinned['shipped_objects'])

    # Only inspected regular ELFs count as shipped objects, not notice files or aliases.
    observed = {label: set() for label in source_only_labels}
    for path in elf_paths:
        owner = owners[path]
        label = owner['owner']
        if label.startswith('ubuntu:'):
            if (('package' in owner or 'version' in owner) and
                    label != f'ubuntu:{owner.get("package")}={owner.get("version")}'):
                raise ValueError(f'Ubuntu owner package/version evidence disagrees with label: {path}: {label}')
            observed.setdefault(label, set()).add(path)
    if observed.keys() != expected.keys():
        raise ValueError(f'Ubuntu component observed identity set mismatch: '
                         f'missing={sorted(expected.keys() - observed.keys())}, '
                         f'unexpected={sorted(observed.keys() - expected.keys())}')
    for label, paths in expected.items():
        actual = observed[label]
        if actual != paths:
            raise ValueError(f'Ubuntu component ELF paths mismatch for {label}: '
                             f'missing={sorted(paths - actual)}, unexpected={sorted(actual - paths)}')
    return observed


def verify_ubuntu_notices(ubuntu, owners, observed, payload_root, digest):
    """Require the packaged copyright bytes for every package that ships an ELF."""
    for row in ubuntu['packages']:
        label = f'ubuntu:{row["binary_package"]}={row["binary_version"]}'
        if not observed.get(label):
            continue
        relative = f'usr/share/doc/ubuntu/{row["binary_package"]}/copyright'
        owner = owners.get(relative)
        if owner is None:
            raise ValueError(f'Ubuntu component extracted copyright has no owner: {relative}')
        if owner.get('owner') != label:
            raise ValueError(f'Ubuntu component extracted copyright owner mismatch: {relative}: {owner.get("owner")}')
        if owner.get('shipped_sha256') != row['notice']['sha256']:
            raise ValueError(f'Ubuntu component extracted copyright bytes differ: {relative}')
        path = payload_root / relative
        if path.is_symlink() or not path.is_file():
            raise ValueError(f'Ubuntu component extracted copyright is not a regular file: {relative}')
        if digest(path) != row['notice']['sha256']:
            raise ValueError(f'Ubuntu component extracted copyright bytes differ: {relative}')


def registry(providers, owners, static, digest, *, payload_root, elf_paths):
    components = {}
    app = json.loads((providers['app'] / 'manifest.json').read_text())
    runtime = json.loads((providers['runtime'] / 'manifest.json').read_text())
    ubuntu = json.loads((providers['stage'] / 'ubuntu-sources.json').read_text())
    static_labels = [f'ubuntu:{item["package"]}={item["version"]}' for item in static['components']]
    crt_labels = [f'ubuntu:{package["name"]}={package["version"]}'
                  for package in (item['binary_package'] for item in app['binary']['direct_objects'])]
    observed = verify_ubuntu_terms(ubuntu, owners, elf_paths, static_labels + crt_labels)
    verify_ubuntu_notices(ubuntu, owners, observed, payload_root, digest)
    package_components = {}
    external_notices = []

    def add(identifier, terms, evidence, **extra):
        if identifier not in components:
            components[identifier] = {'component_id': identifier, 'license_expression': terms,
                                      'license_evidence': evidence, **extra}
        return identifier

    authored = add('furami', 'MIT', 'usr/share/licenses/furami/LICENSE',
                   source_refs=['furami/source-snapshot'], role='authored-source')
    app_components = [authored]
    if 'usr/share/licenses/tabler/LICENSE-tabler.txt' in owners:
        app_components.append(add('tabler-icons', 'MIT', 'usr/share/licenses/tabler/LICENSE-tabler.txt',
                                  source_refs=['furami/assets/icons'], role='embedded-icons'))
    for crate in app['crates']:
        if crate['name'] == 'furami' or crate['role'] == 'not-selected':
            continue
        app_components.append(add(f'cargo:{crate["name"]}@{crate["version"]}', crate.get('license_expression'),
                                  {'cargo_lock_checksum': crate['checksum'], 'vendor': crate['vendor_path']},
                                  role=crate['role'], source_refs=[crate['vendor_path']]))
    for item in app['binary']['rust_standard_library']['archives']:
        app_components.append(add('rust-std:' + item['name'], item['license_expression'],
                                  {'linked_archive_sha256': item['sha256'], 'source': item['source_path']},
                                  role='static-linked-input', source_refs=[app['binary']['rust_standard_library']['source']]))
    for item in app['binary']['direct_objects']:
        app_components.append(add('app-CRT:' + item['binary_package']['name'], item['license_expression'],
                                  item['notice'], role='retained-static-object', source_refs=[item['source_package']],
                                  license_exception=item.get('license_exception')))
    for record in ubuntu['packages']:
        notice = providers['stage'] / 'ubuntu-source-evidence' / record['notice']['path']
        if notice.is_symlink() or digest(notice) != record['notice']['sha256']:
            raise ValueError(f'Ubuntu component notice changed: {record["binary_package"]}')
        identifier = add('ubuntu:' + record['binary_package'] + '@' + record['binary_version'],
                         record['license_expression'],
                         {'notice_sha256': record['notice']['sha256'], 'notice': record['notice']['path'],
                          'kind': 'pinned FUR-025 terms validated against original Debian evidence',
                          'custom_license_texts': record['custom_license_texts'],
                          'custom_license_evidence': record.get('custom_license_evidence', {}),
                          'expression_basis': record['expression_basis'],
                          'terms_lock_sha256': ubuntu['terms_lock_sha256'],
                          'sections': record['license_sections']}, source_refs=record['files'],
                         source_required=record['source_required'], source_package=record['source_package'],
                         version=record['source_version'])
        components[identifier]['license_exception'] = record.get('license_exception')
        components[identifier]['terms_selection_resolved'] = record['terms_selection_resolved']
        package_components[f'ubuntu:{record["binary_package"]}={record["binary_version"]}'] = identifier
        external_notices.append(notice)
    runtime_components = []
    for item in runtime['components']:
        runtime_components.append(add('runtime:' + item['name'], item['spdx'],
                                      {'notices': {name: runtime['notice_files'][name] for name in item['notice_files']},
                                       'linked_archives': runtime['linked_archives'], 'linked_objects': runtime['linked_objects']},
                                      source_refs=[{'url': item['source_url'], 'sha256': item['source_sha256']}],
                                      role='outer-runtime-static'))
    static_components = [package_components[label] for label in static_labels]
    mapped_owners = {}
    for path, owner in owners.items():
        label = owner['owner']
        if label.startswith('ubuntu:'):
            if label not in package_components:
                raise ValueError(f'Ubuntu owner missing source/notice identity: {path}: {label}')
            identifiers = [package_components[label]]
        elif owner.get('components'):
            identifiers = []
            for item in owner['components']:
                identifier = add(item['component_id'], item['license_expression'], item['evidence'],
                                 role='runtime-file')
                identifiers.append(identifier)
            if 'libplacebo' in identifiers:
                identifiers += static_components
        elif label == 'app':
            identifiers = app_components
        elif label == 'source-built-runtime':
            identifiers = runtime_components
        elif label == 'app-crate-source':
            crate = owner['crate']
            identifier = f'cargo:{crate["name"]}@{crate["version"]}'
            if identifier not in app_components:
                raise ValueError(f'crate notice owner missing from active graph: {path}: {identifier}')
            identifiers = [identifier]
        elif label.startswith('rust-std'):
            identifiers = [identifier for identifier in app_components if identifier.startswith('rust-std:')]
        elif label == 'checked-source-notice':
            name = owner['source'].split('.', 1)[0]
            terms = {'mpv': 'GPL-2.0-or-later', 'ffmpeg': 'GPL-2.0-or-later', 'libplacebo': 'LGPL-2.1-or-later'}.get(name)
            identifiers = [add(name, terms, owner['source'])]
        elif label == 'checked-source-patch':
            name = owner['source'].split(':', 1)[0]
            if name != 'ffmpeg':
                raise ValueError('unknown patched source component: ' + name)
            identifiers = [add(name, 'GPL-2.0-or-later', owner['source'])]
        elif label == 'tabler-notice':
            identifiers = ['tabler-icons']
        elif label in ('qualification-template', 'qualification-generated'):
            identifiers = [authored]
        elif label == 'ubuntu-license-text':
            # License text is legal material, not a falsely MIT-attributed code component.
            identifiers = [add('license-text:' + Path(path).name, None, owner['source'], role='verbatim-license-notice')]
        else:
            raise ValueError(f'no actual component provenance: {path}: {label}')
        mapped_owners[path] = [components[identifier] for identifier in dict.fromkeys(identifiers)]
        if not mapped_owners[path]:
            raise ValueError(f'empty actual component mapping: {path}')
    for path, mapped in mapped_owners.items():
        owners[path]['components'] = mapped
    for identifier, record in components.items():
        expression = record['license_expression']
        legal_material = record.get('role') == 'verbatim-license-notice'
        selected = expression
        custom = re.findall(r'LicenseRef-[A-Za-z0-9.-]+', selected or '') if isinstance(selected, str) else []
        evidence = record['license_evidence']
        available_custom = evidence.get('custom_license_texts', {}) if isinstance(evidence, dict) else {}
        record['selected_terms'] = selected
        record['terms_resolved'] = legal_material or bool(selected and selected not in ('NOASSERTION', 'NONE') and
                                                         all(available_custom.get(name) for name in custom) and
                                                         record.get('terms_selection_resolved', True))
        if legal_material:
            record['terms_basis'] = 'verbatim legal/configuration material, not executable code; original terms retained'
    return components, runtime_components, external_notices
